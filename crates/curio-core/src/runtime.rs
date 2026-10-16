//! The per-process runtime: the runner registry and the runtime-level plugins, plus the
//! three ways an agent comes to run here: create, thaw and recover.
//!
//! The host builds a [`RunnerRegistry`] with every runner compiled into the binary, then
//! calls [`Runtime::start`] with the process's [`RuntimeConfig`]. Start loads the secret
//! store, log store and provisioner, and checks the allow-list of runners agents may use.
//! After that the runtime is read-only and holds no agent state; any number of agents can
//! be started from it.
//!
//! Each way of starting an agent ends the same way: load every plugin in the agent's config
//! through its runner, build an [`AgentCtx`] around the opened log, attach the agent's
//! queue consumer, and spawn a task that runs the driver. They differ in where the state
//! comes from:
//!
//! - [`Runtime::create`] asks the provisioner for a new [`crate::AgentSpec`], writes the
//!   log header, and logs the spec's initial events.
//! - [`Runtime::thaw`] takes an [`AgentImage`]: it continues the log from the image's
//!   position (refusing a stale image), restores every state slot, migrating old ones, and
//!   takes the view from the image without reading the log.
//! - [`Runtime::recover`] reads the whole log, replays every event into the view and
//!   through the `replay` hook, and closes any turn the crash interrupted with
//!   [`repair_after_crash`].
//!
//! Thaw and recover both raise the log's epoch, which fences off whichever host had the
//! agent before. Each logs a `Thawed` or `Recovered` marker and syncs before the driver
//! starts. Neither dispatches `agent_loaded`; the driver does that first thing in its run.

use crate::agent::{AgentCtx, AgentHandle, Command, LoadKind, Observation, RunningAgent};
use crate::config::{AgentConfig, ResumeConfig, RuntimeConfig, RuntimePluginSpec};
use crate::error::{Error, Result};
use crate::event::{Delivery, Event, Source, StepOutcome, TurnEnd, View};
use crate::hook::{HookEvent, dispatch};
use crate::ids::{AgentId, PluginId, TenantId};
use crate::image::{AgentImage, IMAGE_FORMAT};
use crate::log::{Journal, LOG_FORMAT, LogHeader, LogStore, OpenMode};
use crate::message::ToolResult;
use crate::plugin::{
    CreateRequest, LoadEnv, Plugin, PluginEntry, PluginState, PluginTable, Provisioner, Runner,
    SecretScope,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Shown to the model at the start of a turn that continues after a crash. Queued as a
/// system notice by [`repair_after_crash`] when the agent's resume policy allows it.
pub const RESTARTED_NOTICE: &str = "The previous turn was interrupted because the agent's host stopped unexpectedly. \
Results of interrupted tool calls are marked in the conversation. Continue the task from where it stopped.";

/// The synthetic result for a call whose `ToolStarted` was logged: it may have run.
const STARTED_CALL_REPAIR: &str = "The host stopped while this tool was running. Its outcome is unknown. Retry only \
if the operation is read-only or idempotent; otherwise verify external state or ask the user first.";
/// The synthetic result for a call that never got a `ToolStarted`: it didn't run.
const UNSTARTED_CALL_REPAIR: &str =
    "The host stopped before this tool ran. It was not run. Retry it if it is still needed.";

/// Runners compiled into this process, by runner name.
///
/// Filled by the host before start and moved into the [`Runtime`], which never changes it.
#[derive(Default)]
pub struct RunnerRegistry {
    /// Runner name to runner.
    runners: HashMap<String, Arc<dyn Runner>>,
}

impl RunnerRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a runner under its name. Fails if the name is taken.
    pub fn register(&mut self, runner: impl Runner) -> Result<()> {
        let name = runner.name().to_string();
        if self.runners.contains_key(&name) {
            return Err(Error::config(format!(
                "runner `{name}` is registered twice"
            )));
        }
        self.runners.insert(name, Arc::new(runner));
        Ok(())
    }

    /// The runner registered under `name`, whether or not agents are allowed to use it.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Runner>> {
        self.runners.get(name)
    }

    /// Every registered runner name, sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<_> = self.runners.keys().map(String::as_str).collect();
        names.sort();
        names
    }
}

/// Shared, immutable machinery for every agent in the process. Holds no agent state.
///
/// It doesn't track the agents it starts either: each call returns a [`RunningAgent`], and
/// keeping the handles is the host's job.
pub struct Runtime {
    /// Written into `Thawed` and `Recovered` events.
    host_id: String,
    /// Every runner in the process.
    registry: RunnerRegistry,
    /// Runner names and `prefix*` patterns agent plugins may use.
    enabled: Vec<String>,
    /// What relative paths in plugin code resolve against.
    base_dir: PathBuf,
    /// The runtime-level log store; checked at start to provide one.
    log_store: Arc<dyn Plugin>,
    /// The runtime-level provisioner; checked at start to provide one.
    provisioner: Arc<dyn Plugin>,
    /// The runtime-level secret store; checked at start to provide one.
    secret_store: Arc<dyn Plugin>,
}

impl Runtime {
    /// Loads the runtime-level plugins. Relative paths in any plugin's code resolve against
    /// `base_dir`.
    ///
    /// The secret store loads first, with no secrets of its own; the log store and the
    /// provisioner then get scopes over it for the `host` tenant. Fails if a runner is
    /// unknown, a plugin fails to load or doesn't provide its service, or an entry in
    /// `runners` matches no registered runner.
    pub async fn start(
        config: RuntimeConfig,
        registry: RunnerRegistry,
        base_dir: PathBuf,
    ) -> Result<Self> {
        let host_tenant = TenantId::new("host");
        let load = async |id: &str,
                          spec: &RuntimePluginSpec,
                          secrets: Option<Arc<dyn Plugin>>|
               -> Result<Arc<dyn Plugin>> {
            let runner = registry
                .get(&spec.runner)
                .ok_or_else(|| Error::UnknownRunner(spec.runner.clone()))?;
            let env = LoadEnv {
                plugin: PluginId::new(id),
                agent: None,
                tenant: host_tenant.clone(),
                grants: spec.grants.clone(),
                base_dir: base_dir.clone(),
                secrets: SecretScope::new(
                    secrets,
                    host_tenant.clone(),
                    spec.grants.secrets.clone(),
                ),
            };
            Ok(runner.load(&spec.code, &env).await?.instance)
        };
        let secret_store = load("secret_store", &config.secret_store, None).await?;
        if secret_store.as_secret_store().is_none() {
            return Err(Error::config(
                "the secret_store plugin does not provide a secret store",
            ));
        }
        let log_store = load("log_store", &config.log_store, Some(secret_store.clone())).await?;
        if log_store.as_log_store().is_none() {
            return Err(Error::config(
                "the log_store plugin does not provide a log store",
            ));
        }
        let provisioner = load(
            "provisioner",
            &config.provisioner,
            Some(secret_store.clone()),
        )
        .await?;
        if provisioner.as_provisioner().is_none() {
            return Err(Error::config(
                "the provisioner plugin does not provide a provisioner",
            ));
        }
        for pattern in &config.runners {
            let known = match pattern.strip_suffix('*') {
                Some(prefix) => registry.names().iter().any(|n| n.starts_with(prefix)),
                None => registry.get(pattern).is_some(),
            };
            if !known {
                return Err(Error::config(format!(
                    "`runners` names `{pattern}`, which matches no registered runner"
                )));
            }
        }
        Ok(Self {
            host_id: config.host_id,
            registry,
            enabled: config.runners,
            base_dir,
            log_store,
            provisioner,
            secret_store,
        })
    }

    /// This host's id, as written in `Thawed` and `Recovered` events.
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// The runtime-level log store. Hosts use it to check whether an agent exists before
    /// choosing between create and recover.
    pub fn log_store(&self) -> &dyn LogStore {
        self.log_store.as_log_store().expect("checked at start")
    }

    /// The runtime-level provisioner.
    fn provisioner(&self) -> &dyn Provisioner {
        self.provisioner.as_provisioner().expect("checked at start")
    }

    /// The runner for an agent plugin. Fails with [`Error::UnknownRunner`] unless it's both
    /// registered and enabled by the `runners` list.
    fn agent_runner(&self, name: &str) -> Result<&Arc<dyn Runner>> {
        let enabled = self.enabled.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => p == name,
        });
        match self.registry.get(name) {
            Some(runner) if enabled => Ok(runner),
            _ => Err(Error::UnknownRunner(name.to_string())),
        }
    }

    /// Loads every plugin in `config` through its runner, in config order, and indexes them.
    /// Returns the table and each plugin's initial state slot, in the same order. Load
    /// errors are attributed to the failing plugin.
    async fn load_plugins(
        &self,
        config: &AgentConfig,
    ) -> Result<(PluginTable, Vec<Box<dyn PluginState>>)> {
        let mut entries = Vec::with_capacity(config.plugins.len());
        let mut states = Vec::with_capacity(config.plugins.len());
        for spec in &config.plugins {
            let runner = self.agent_runner(&spec.runner)?;
            let env = LoadEnv {
                plugin: spec.id.clone(),
                agent: Some(config.id.clone()),
                tenant: config.tenant.clone(),
                grants: spec.grants.clone(),
                base_dir: self.base_dir.clone(),
                secrets: SecretScope::new(
                    Some(self.secret_store.clone()),
                    config.tenant.clone(),
                    spec.grants.secrets.clone(),
                ),
            };
            let loaded = runner.load(&spec.code, &env).await.map_err(|e| match e {
                Error::Plugin { .. } => e,
                other => Error::plugin(&spec.id, other),
            })?;
            entries.push(PluginEntry {
                id: spec.id.clone(),
                runner: spec.runner.clone(),
                state_version: runner.state_version(),
                grants: spec.grants.clone(),
                plugin: loaded.instance,
            });
            states.push(loaded.initial_state);
        }
        Ok((PluginTable::build(entries)?, states))
    }

    /// Creates a new agent from the provisioner's output and starts it.
    ///
    /// Plugins load before the log is created, so a misconfigured agent leaves no log
    /// behind. Fails with [`Error::AgentExists`] if the id already has a log.
    pub async fn create(&self, request: CreateRequest) -> Result<RunningAgent> {
        let spec = self.provisioner().provision(request).await?;
        let config = spec.config;
        if self.log_store().exists(&config.id).await? {
            return Err(Error::AgentExists(config.id));
        }
        let (table, mut states) = self.load_plugins(&config).await?;
        for (plugin, blob) in spec.initial_state {
            let index = table.index_of(&plugin).ok_or_else(|| {
                Error::config(format!("initial state names unknown plugin `{plugin}`"))
            })?;
            states[index] = table.entries()[index].plugin.decode_state(blob)?;
        }
        let header = LogHeader {
            format: LOG_FORMAT,
            config: config.clone(),
            state_versions: table
                .entries()
                .iter()
                .map(|e| (e.id.clone(), e.state_version))
                .collect(),
            created_at_ms: crate::now_ms(),
        };
        let opened = self
            .log_store()
            .open(&config.id, OpenMode::Create(header))
            .await?;
        let journal = Journal::new(opened.log, View::default());
        let (mut cx, tx) = AgentCtx::new(
            config,
            self.host_id.clone(),
            table,
            states,
            journal,
            LoadKind::Created,
        );
        for event in spec.initial_events {
            cx.journal.append(event)?;
        }
        cx.journal.sync().await?;
        self.attach_and_spawn(cx, tx).await
    }

    /// Restores a quiesced agent from its image and starts it. Fails with
    /// [`Error::StaleImage`] if the agent's log has moved past the image.
    ///
    /// The log is opened, and its epoch raised, before plugins load. If loading then fails,
    /// the previous owner is still fenced off, and the same image can be thawed again once
    /// the problem is fixed, since nothing was appended. The view comes from the image, so
    /// a step-boundary drain's open turn is still open and the driver continues it.
    pub async fn thaw(&self, image: AgentImage) -> Result<RunningAgent> {
        if image.format != IMAGE_FORMAT {
            return Err(Error::config(format!(
                "unsupported image format {}",
                image.format
            )));
        }
        let config = image.config;
        let opened = self
            .log_store()
            .open(&config.id, OpenMode::ContinueFrom(image.log_position))
            .await?;
        let (table, mut states) = self.load_plugins(&config).await?;
        if image.slots.len() != table.entries().len() {
            return Err(Error::config(
                "the image does not have exactly one state slot per plugin",
            ));
        }
        for slot in image.slots {
            let index = table.index_of(&slot.plugin).ok_or_else(|| {
                Error::config(format!(
                    "the image has a slot for unknown plugin `{}`",
                    slot.plugin
                ))
            })?;
            let entry = &table.entries()[index];
            let blob = if slot.version == entry.state_version {
                slot.blob
            } else if slot.version < entry.state_version {
                self.agent_runner(&entry.runner)?
                    .migrate(slot.blob, slot.version)?
            } else {
                return Err(Error::plugin(
                    &entry.id,
                    format!(
                        "slot version {} is newer than its runner's {}",
                        slot.version, entry.state_version
                    ),
                ));
            };
            states[index] = entry.plugin.decode_state(blob)?;
        }
        let journal = Journal::new(opened.log, image.view);
        let (mut cx, tx) = AgentCtx::new(
            config,
            self.host_id.clone(),
            table,
            states,
            journal,
            LoadKind::Thawed,
        );
        let epoch = cx.journal.position().epoch;
        cx.journal.append(Event::Thawed {
            host: self.host_id.clone(),
            epoch,
        })?;
        cx.journal.sync().await?;
        self.attach_and_spawn(cx, tx).await
    }

    /// Rebuilds an agent that stopped without quiescing from its log alone, repairs any
    /// interrupted turn, and starts it.
    ///
    /// Plugins start from their runners' initial state and rebuild anything they need from
    /// the `replay` hook, which sees each event right after it's folded. During replay,
    /// hooks can't append. Recovering an agent that did quiesce cleanly is also safe: its
    /// last turn is either closed or suspended, so nothing is repaired.
    pub async fn recover(&self, id: &AgentId) -> Result<RunningAgent> {
        let opened = self.log_store().open(id, OpenMode::Recover).await?;
        let config = opened.header.config.clone();
        let (table, states) = self.load_plugins(&config).await?;
        let journal = Journal::new(opened.log, View::default());
        let (mut cx, tx) = AgentCtx::new(
            config,
            self.host_id.clone(),
            table,
            states,
            journal,
            LoadKind::Recovered,
        );
        cx.replaying = true;
        for event in &opened.events {
            cx.journal.replay(event);
            dispatch(&mut cx, &mut HookEvent::Replay { event }).await?;
        }
        cx.replaying = false;
        let repaired_calls = repair_after_crash(&mut cx)?;
        let epoch = cx.journal.position().epoch;
        cx.journal.append(Event::Recovered {
            host: self.host_id.clone(),
            epoch,
            repaired_calls,
        })?;
        cx.journal.sync().await?;
        self.attach_and_spawn(cx, tx).await
    }

    /// Attaches the agent to its queue, makes its handle, and spawns the task that runs its
    /// driver. When the driver fails, the task tells observers and drain waiters before
    /// returning the error.
    async fn attach_and_spawn(
        &self,
        mut cx: AgentCtx,
        tx: mpsc::Sender<Command>,
    ) -> Result<RunningAgent> {
        let queue = cx.plugins.queue().clone();
        let queue = queue
            .as_queue()
            .ok_or_else(|| Error::config("queue plugin has no queue service"))?;
        cx.queue = Some(queue.attach(&cx.id).await?);
        let handle = AgentHandle::new(&cx, tx);
        let task = tokio::spawn(async move {
            let driver = cx.plugins.driver().clone();
            let driver = driver.as_driver().expect("checked by PluginTable::build");
            let result = driver.run(&mut cx).await;
            if let Err(e) = &result {
                cx.observer
                    .emit(Observation::Notice(format!("agent stopped: {e}")));
                cx.fail_drain_waiters(&e.to_string());
            }
            result
        });
        Ok(RunningAgent { handle, task })
    }
}

/// Closes a turn a crash interrupted: every tool call without a result gets an honest
/// synthetic one, then the step and turn are ended as interrupted. With
/// `resume.after_crash: continue`, queues a notice that starts the next turn. Does nothing
/// when no turn is open or the open turn was suspended by a drain. Returns how many calls
/// were repaired.
///
/// A call that has a `ToolStarted` may have run, so its result says the outcome is unknown;
/// a call without one says it wasn't run. The notice is queued only while
/// [`crate::event::Counters::crash_resumes`] (which already counts this turn) is at most
/// `max_resumes`, so a turn that keeps crashing the host stops resuming itself. The events
/// are appended but not synced; the caller syncs.
pub fn repair_after_crash(cx: &mut AgentCtx) -> Result<usize> {
    let view = cx.journal.view();
    let Some(turn) = view.open_turn else {
        return Ok(0);
    };
    if view.suspended {
        return Ok(0);
    }
    let dangling = view.pending_calls.clone();
    let open_step = view.open_step;
    for pending in &dangling {
        let text = if pending.started {
            STARTED_CALL_REPAIR
        } else {
            UNSTARTED_CALL_REPAIR
        };
        cx.journal.append(Event::ToolResult {
            result: ToolResult::synthetic(&pending.call, text),
        })?;
    }
    if open_step {
        cx.journal.append(Event::StepEnded {
            outcome: StepOutcome::Interrupted,
        })?;
    }
    cx.journal.append(Event::TurnEnded {
        turn,
        end: TurnEnd::Interrupted,
    })?;
    if let ResumeConfig::Continue { max_resumes } = cx.config.resume
        && cx.journal.view().counters.crash_resumes <= max_resumes
    {
        cx.journal.append(Event::ContextQueued {
            source: Source::System,
            delivery: Delivery::NextTurn,
            text: RESTARTED_NOTICE.to_string(),
        })?;
    }
    Ok(dangling.len())
}
