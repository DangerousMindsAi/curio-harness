//! End-to-end tests of the loop with the built-in plugins: turns, tools, approval,
//! quiesce and thaw, drains at a step boundary, crash recovery, fencing and isolation.

use curio_builtins::registry;
use curio_core::{
    AgentId, AgentImage, Answer, CreateRequest, Delivery, DrainPoint, DrainResult, Error, Event,
    Observation, OpenMode, Role, RunningAgent, Runtime, RuntimeConfig, RuntimePluginSpec, TurnEnd,
};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tokio::sync::broadcast;

const WAIT: Duration = Duration::from_secs(10);

fn spec(runner: &str, code: Value) -> RuntimePluginSpec {
    RuntimePluginSpec {
        runner: runner.into(),
        code,
        grants: Default::default(),
    }
}

async fn runtime_with_secrets(dir: &Path, secrets: Value) -> Runtime {
    let config = RuntimeConfig {
        host_id: "test-host".into(),
        runners: vec!["builtin.*".into()],
        log_store: spec("builtin.logstore.file", json!({ "dir": "logs" })),
        provisioner: spec("builtin.provisioner.yaml-dir", json!({ "dir": "agents" })),
        secret_store: spec("builtin.secrets.inline", json!({ "secrets": secrets })),
    };
    Runtime::start(config, registry().unwrap(), dir.to_path_buf())
        .await
        .unwrap()
}

async fn runtime(dir: &Path) -> Runtime {
    runtime_with_secrets(dir, json!({})).await
}

fn write_agent(dir: &Path, name: &str, yaml: &str) {
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(dir.join("agents").join(format!("{name}.yaml")), yaml).unwrap();
}

/// A scripted agent: `replies` is the YAML list of scripted replies; `extra` adds plugins.
fn scripted_agent(
    id: &str,
    tenant: &str,
    resume: &str,
    delay_ms: u64,
    replies: &str,
    extra: &str,
) -> String {
    format!(
        r#"
id: {id}
tenant: {tenant}
resume: {resume}
plugins:
  - id: driver
    runner: builtin.driver.react
    code: {{ model: model, max_steps_per_turn: 10, max_forced_continuations: 2 }}
  - id: model
    runner: builtin.model.scripted
    code:
      delay_ms: {delay_ms}
      replies:
{replies}
  - id: queue
    runner: builtin.queue.file
    code: {{ dir: queues }}
  - id: instructions
    runner: builtin.instructions
    code: {{ text: "Be brief." }}
  - id: clock
    runner: builtin.tool.clock
{extra}
"#
    )
}

const CLOCK_THEN_ANSWER: &str = r#"
        - text: "Checking."
          tool_calls: [{ name: clock }]
        - text: "It is now."
        - text: "Second answer."
"#;

async fn create(rt: &Runtime, template: &str) -> RunningAgent {
    rt.create(CreateRequest {
        template: template.into(),
        agent_id: None,
        params: Value::Null,
    })
    .await
    .unwrap()
}

async fn next_turn_end(rx: &mut broadcast::Receiver<Observation>) -> TurnEnd {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Observation::TurnEnded { end, .. } = rx.recv().await.unwrap() {
                return end;
            }
        }
    })
    .await
    .expect("turn did not end")
}

async fn wait_for(
    rx: &mut broadcast::Receiver<Observation>,
    pred: impl Fn(&Observation) -> bool,
) -> Observation {
    tokio::time::timeout(WAIT, async {
        loop {
            let o = rx.recv().await.unwrap();
            if pred(&o) {
                return o;
            }
        }
    })
    .await
    .expect("observation did not arrive")
}

async fn drain(agent: RunningAgent, at: DrainPoint) -> AgentImage {
    let result = agent.handle.drain(at, WAIT).await.unwrap();
    agent.task.await.unwrap().unwrap();
    match result {
        DrainResult::Quiesced { image, forced } => {
            assert!(!forced);
            *image
        }
        DrainResult::Failed(e) => panic!("drain failed: {e}"),
    }
}

fn log_events(dir: &Path, agent: &str) -> Vec<Event> {
    let text = std::fs::read_to_string(dir.join("logs").join(agent).join("log.jsonl")).unwrap();
    text.lines()
        .skip(1)
        .map(|l| {
            serde_json::from_value(serde_json::from_str::<Value>(l).unwrap()["event"].clone())
                .unwrap()
        })
        .collect()
}

fn contents(image: &AgentImage) -> Vec<(Role, String)> {
    image
        .view
        .history
        .messages
        .iter()
        .map(|m| (m.role, m.content.clone()))
        .collect()
}

#[tokio::test]
async fn a_turn_runs_tools_and_answers() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        &scripted_agent("a", "t1", "{ after_crash: wait }", 0, CLOCK_THEN_ANSWER, ""),
    );
    let rt = runtime(dir.path()).await;
    let agent = create(&rt, "a").await;
    let mut rx = agent.handle.subscribe();
    agent
        .handle
        .send(Delivery::NextTurn, "What time is it?")
        .await
        .unwrap();
    assert_eq!(next_turn_end(&mut rx).await, TurnEnd::Completed);

    let image = drain(agent, DrainPoint::Turn).await;
    assert_eq!(image.view.history.system[0].1, "Be brief.");
    let roles: Vec<_> = contents(&image).into_iter().map(|(r, _)| r).collect();
    assert_eq!(
        roles,
        [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(contents(&image)[3].1, "It is now.");
    let instructions = image
        .slots
        .iter()
        .find(|s| s.plugin.as_str() == "instructions")
        .unwrap();
    assert_eq!(instructions.blob, json!({ "applied": true }));
}

#[tokio::test]
async fn quiesce_then_thaw_on_another_runtime_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        &scripted_agent("a", "t1", "{ after_crash: wait }", 0, CLOCK_THEN_ANSWER, ""),
    );
    let image = {
        let rt = runtime(dir.path()).await;
        let agent = create(&rt, "a").await;
        let mut rx = agent.handle.subscribe();
        agent
            .handle
            .send(Delivery::NextTurn, "What time is it?")
            .await
            .unwrap();
        next_turn_end(&mut rx).await;
        let handle = agent.handle.clone();
        let image = drain(agent, DrainPoint::Turn).await;
        // Sent while the agent is quiesced: it waits in the queue.
        handle.send(Delivery::NextTurn, "And again?").await.unwrap();
        image
    };
    // The image travels as data.
    let image: AgentImage = serde_json::from_str(&serde_json::to_string(&image).unwrap()).unwrap();

    let rt = runtime(dir.path()).await;
    let agent = rt.thaw(image).await.unwrap();
    let mut rx = agent.handle.subscribe();
    assert_eq!(next_turn_end(&mut rx).await, TurnEnd::Completed);
    let image = drain(agent, DrainPoint::Turn).await;
    let last = contents(&image).pop().unwrap();
    assert_eq!(last, (Role::Assistant, "Second answer.".to_string()));
    assert_eq!(contents(&image)[4].1, "And again?");

    let events = log_events(dir.path(), "a");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::SystemPrompt { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Thawed { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::TurnStarted { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn a_drain_at_a_step_boundary_suspends_the_turn_and_thaw_continues_it() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        &scripted_agent(
            "a",
            "t1",
            "{ after_crash: wait }",
            300,
            CLOCK_THEN_ANSWER,
            "",
        ),
    );
    let image = {
        let rt = runtime(dir.path()).await;
        let agent = create(&rt, "a").await;
        let mut rx = agent.handle.subscribe();
        agent
            .handle
            .send(Delivery::NextTurn, "What time is it?")
            .await
            .unwrap();
        wait_for(&mut rx, |o| matches!(o, Observation::TurnStarted { .. })).await;
        drain(agent, DrainPoint::Step).await
    };
    assert!(image.view.open_turn.is_some() && image.view.suspended);

    let rt = runtime(dir.path()).await;
    let agent = rt.thaw(image).await.unwrap();
    let mut rx = agent.handle.subscribe();
    assert_eq!(next_turn_end(&mut rx).await, TurnEnd::Completed);
    let image = drain(agent, DrainPoint::Turn).await;
    assert_eq!(contents(&image).pop().unwrap().1, "It is now.");
    let events = log_events(dir.path(), "a");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::TurnStarted { .. }))
            .count(),
        1
    );
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Quiesced {
            suspended_turn: Some(_),
            ..
        }
    )));
}

#[tokio::test]
async fn a_crash_during_a_tool_is_repaired_and_the_turn_continues() {
    let dir = tempfile::tempdir().unwrap();
    let replies = r#"
        - text: "Sleeping."
          tool_calls: [{ name: shell, arguments: { command: "sleep 30" } }]
        - text: "Recovered fine."
"#;
    let shell = "  - id: shell\n    runner: builtin.tool.shell\n    grants: { fs: [work] }\n";
    let yaml = scripted_agent(
        "a",
        "t1",
        "{ after_crash: continue, max_resumes: 1 }",
        50,
        replies,
        shell,
    );
    write_agent(dir.path(), "a", &yaml);
    {
        let rt = runtime(dir.path()).await;
        let agent = create(&rt, "a").await;
        let mut rx = agent.handle.subscribe();
        agent
            .handle
            .send(Delivery::NextTurn, "Sleep, please.")
            .await
            .unwrap();
        wait_for(&mut rx, |o| matches!(o, Observation::ToolCalled { .. })).await;
        agent.task.abort(); // the process dies mid-tool
        let _ = agent.task.await;
    }

    let rt = runtime(dir.path()).await;
    let agent = rt.recover(&AgentId::new("a")).await.unwrap();
    let mut rx = agent.handle.subscribe();
    assert_eq!(next_turn_end(&mut rx).await, TurnEnd::Completed);
    let image = drain(agent, DrainPoint::Turn).await;

    let messages = contents(&image);
    assert!(messages[2].1.contains("outcome is unknown"), "{messages:?}");
    assert!(messages[3].1.starts_with("[system notice]"));
    assert_eq!(messages.last().unwrap().1, "Recovered fine.");
    let events = log_events(dir.path(), "a");
    assert!(events.iter().any(|e| matches!(
        e,
        Event::TurnEnded {
            end: TurnEnd::Interrupted,
            ..
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Recovered {
            repaired_calls: 1,
            ..
        }
    )));
}

#[tokio::test]
async fn a_newer_owner_fences_off_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        &scripted_agent("a", "t1", "{ after_crash: wait }", 0, CLOCK_THEN_ANSWER, ""),
    );
    let rt = runtime(dir.path()).await;
    let agent = create(&rt, "a").await;
    // Another host takes the log over.
    let other = runtime(dir.path()).await;
    let _taken = other
        .log_store()
        .open(&AgentId::new("a"), OpenMode::Recover)
        .await
        .unwrap();
    agent
        .handle
        .send(Delivery::NextTurn, "Hello?")
        .await
        .unwrap();
    let result = tokio::time::timeout(WAIT, agent.task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(Error::Fenced { .. })), "{result:?}");
}

#[tokio::test]
async fn an_out_of_date_image_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        &scripted_agent("a", "t1", "{ after_crash: wait }", 0, CLOCK_THEN_ANSWER, ""),
    );
    let rt = runtime(dir.path()).await;
    let agent = create(&rt, "a").await;
    let old = drain(agent, DrainPoint::Turn).await;
    let agent = rt.thaw(old.clone()).await.unwrap();
    let mut rx = agent.handle.subscribe();
    agent.handle.send(Delivery::NextTurn, "Hi").await.unwrap();
    next_turn_end(&mut rx).await;
    drain(agent, DrainPoint::Turn).await;
    assert!(matches!(rt.thaw(old).await, Err(Error::StaleImage { .. })));
}

#[tokio::test]
async fn approval_can_deny_a_call() {
    let dir = tempfile::tempdir().unwrap();
    let replies = r#"
        - text: ""
          tool_calls: [{ name: shell, arguments: { command: "echo hi" } }]
        - text: "Understood."
"#;
    let extra = "  - id: shell\n    runner: builtin.tool.shell\n    grants: { fs: [work] }\n  - id: approval\n    runner: builtin.approval\n    code: { ask: [shell] }\n";
    write_agent(
        dir.path(),
        "a",
        &scripted_agent("a", "t1", "{ after_crash: wait }", 0, replies, extra),
    );
    let rt = runtime(dir.path()).await;
    let agent = create(&rt, "a").await;
    let mut rx = agent.handle.subscribe();
    agent
        .handle
        .send(Delivery::NextTurn, "Say hi")
        .await
        .unwrap();
    let Observation::ApprovalRequested { request, .. } = wait_for(&mut rx, |o| {
        matches!(o, Observation::ApprovalRequested { .. })
    })
    .await
    else {
        unreachable!()
    };
    agent
        .handle
        .answer(
            request,
            Answer {
                approve: false,
                note: Some("not now".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(next_turn_end(&mut rx).await, TurnEnd::Completed);
    let image = drain(agent, DrainPoint::Turn).await;
    assert_eq!(contents(&image)[2].1, "The user denied this call: not now");
}

#[tokio::test]
async fn agents_in_one_process_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        &scripted_agent(
            "a",
            "acme",
            "{ after_crash: wait }",
            20,
            CLOCK_THEN_ANSWER,
            "",
        ),
    );
    let other = "\n        - text: \"I am B.\"\n";
    write_agent(
        dir.path(),
        "b",
        &scripted_agent("b", "globex", "{ after_crash: wait }", 20, other, ""),
    );
    let rt = runtime(dir.path()).await;
    let (a, b) = (create(&rt, "a").await, create(&rt, "b").await);
    let (mut ra, mut rb) = (a.handle.subscribe(), b.handle.subscribe());
    a.handle.send(Delivery::NextTurn, "for a").await.unwrap();
    b.handle.send(Delivery::NextTurn, "for b").await.unwrap();
    let (ea, eb) = tokio::join!(next_turn_end(&mut ra), next_turn_end(&mut rb));
    assert_eq!((ea, eb), (TurnEnd::Completed, TurnEnd::Completed));
    let (ia, ib) = tokio::join!(drain(a, DrainPoint::Turn), drain(b, DrainPoint::Turn));
    assert_eq!(contents(&ia)[0].1, "for a");
    assert_eq!(
        contents(&ib),
        [
            (Role::User, "for b".into()),
            (Role::Assistant, "I am B.".into())
        ]
    );
}

/// Serves one canned HTTP response per connection, in order, and records each request.
async fn fake_server(responses: Vec<String>) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        requests.push(text);
                        break;
                    }
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });
    (port, task)
}

#[tokio::test]
async fn the_openai_compatible_model_streams_retries_and_sends_the_key() {
    let sse = [
        r#"{"choices":[{"delta":{"content":"Hello "}}]}"#,
        r#"{"choices":[{"delta":{"content":"there."},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
        "[DONE]",
    ]
    .iter()
    .map(|d| format!("data: {d}\n\n"))
    .collect::<String>();
    let (port, server) = fake_server(vec![
        "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy"
            .into(),
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{sse}"
        ),
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let yaml = format!(
        r#"
id: a
tenant: t1
resume: {{ after_crash: wait }}
plugins:
  - id: driver
    runner: builtin.driver.react
    code: {{ model: model, max_steps_per_turn: 4, max_forced_continuations: 1 }}
  - id: model
    runner: builtin.model.openai-compat
    code: {{ base_url: "http://127.0.0.1:{port}/v1/openai", model: test-model, api_key_secret: key }}
    grants: {{ hosts: ["127.0.0.1"], secrets: [key] }}
  - id: retry
    runner: builtin.retry
    code: {{ max_retries: 2, base_delay_ms: 10 }}
  - id: queue
    runner: builtin.queue.file
    code: {{ dir: queues }}
"#
    );
    write_agent(dir.path(), "a", &yaml);
    let rt = runtime_with_secrets(dir.path(), json!({ "t1/key": "sk-test" })).await;
    let agent = create(&rt, "a").await;
    let mut rx = agent.handle.subscribe();
    agent.handle.send(Delivery::NextTurn, "Hi").await.unwrap();
    assert_eq!(next_turn_end(&mut rx).await, TurnEnd::Completed);
    let image = drain(agent, DrainPoint::Turn).await;
    assert_eq!(contents(&image).pop().unwrap().1, "Hello there.");
    assert_eq!(image.view.counters.completion_tokens, 2);

    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].starts_with("POST /v1/openai/chat/completions"));
    assert!(
        requests[1]
            .to_lowercase()
            .contains("authorization: bearer sk-test")
    );
    assert!(requests[1].contains(r#""model":"test-model""#));
}

#[tokio::test]
async fn a_secret_that_is_not_granted_fails_at_creation() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
id: a
tenant: t1
resume: { after_crash: wait }
plugins:
  - id: driver
    runner: builtin.driver.react
    code: { model: model, max_steps_per_turn: 4, max_forced_continuations: 1 }
  - id: model
    runner: builtin.model.openai-compat
    code: { base_url: "https://api.deepinfra.com/v1/openai", model: m, api_key_secret: key }
    grants: { hosts: [api.deepinfra.com] }
  - id: queue
    runner: builtin.queue.file
    code: { dir: queues }
"#;
    write_agent(dir.path(), "a", yaml);
    let rt = runtime_with_secrets(dir.path(), json!({ "t1/key": "sk-test" })).await;
    let err = rt
        .create(CreateRequest {
            template: "a".into(),
            agent_id: None,
            params: Value::Null,
        })
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("secret `key`"), "{err}");
}
