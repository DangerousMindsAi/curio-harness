//! The terminal conversation with one agent.

use crate::save_image;
use anyhow::Result;
use curio_core::{
    Answer, Delivery, DrainPoint, DrainResult, Observation, RequestId, RunningAgent, TurnEnd,
};
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast::error::RecvError;

const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

const HELP: &str = "Type a message and press Enter. While the agent is working, what you type is added to its next step.
  /quit    finish the current turn, save the agent and exit (Ctrl-D does the same)
  /pause   stop at the next step boundary, save the agent mid-turn and exit
  /cancel  cancel the current turn (Ctrl-C does the same while a turn runs)
  /kill    exit at once without saving, as a crash would; the next start recovers from the log
  /help    show this";

/// How long `/quit` and `/pause` wait before forcing the agent to stop.
const DRAIN_DEADLINE: Duration = Duration::from_secs(120);

fn prompt() {
    print!("{CYAN}> {RESET}");
    let _ = std::io::stdout().flush();
}

fn short(text: &str, max: usize) -> String {
    let one_line = text.replace('\n', " ⏎ ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    }
}

/// What the terminal shows and expects next.
#[derive(Default)]
struct Screen {
    busy: bool,
    approval: Option<RequestId>,
    mid_line: bool,
}

impl Screen {
    fn line_break(&mut self) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
    }

    fn show(&mut self, observation: Observation) {
        match observation {
            Observation::TurnStarted { .. } => self.busy = true,
            Observation::TextDelta(text) => {
                print!("{text}");
                self.mid_line = !text.ends_with('\n');
            }
            Observation::ReasoningDelta(text) => {
                print!("{DIM}{text}{RESET}");
                self.mid_line = !text.ends_with('\n');
            }
            Observation::ToolCalled {
                name, arguments, ..
            } => {
                self.line_break();
                println!("{DIM}  ⚙ {name} {}{RESET}", short(&arguments, 160));
            }
            Observation::ToolFinished {
                content, is_error, ..
            } => {
                let color = if is_error { RED } else { DIM };
                println!("{color}  ↳ {}{RESET}", short(&content, 240));
            }
            Observation::ApprovalRequested {
                request, question, ..
            } => {
                self.line_break();
                print!(
                    "{CYAN}  ? {} [y/N, or a reason to refuse] {RESET}",
                    short(&question, 200)
                );
                self.approval = Some(request);
            }
            Observation::Retrying { error } => {
                println!("{DIM}  (retrying after: {}){RESET}", short(&error, 200))
            }
            Observation::Notice(text) => println!("{DIM}  ({text}){RESET}"),
            Observation::TurnEnded { end, .. } => {
                self.busy = false;
                self.approval = None;
                self.line_break();
                if end != TurnEnd::Completed {
                    println!("{RED}  (turn ended: {end:?}){RESET}");
                }
                prompt();
            }
            Observation::Quiesced { .. } => {}
        }
        let _ = std::io::stdout().flush();
    }
}

/// Runs the conversation until the user quits or the agent stops.
pub async fn run(mut agent: RunningAgent, images_dir: &Path) -> Result<()> {
    let handle = agent.handle.clone();
    let mut observations = handle.subscribe();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut screen = Screen::default();

    println!(
        "{DIM}Talking to `{}`. /help lists commands.{RESET}",
        handle.id
    );
    prompt();
    let drain_at = loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break DrainPoint::Turn };
                let line = line.trim().to_string();
                if let Some(request) = screen.approval.take() {
                    let approve = matches!(line.to_lowercase().as_str(), "y" | "yes");
                    let note = (!approve && !matches!(line.to_lowercase().as_str(), "" | "n" | "no")).then_some(line);
                    handle.answer(request, Answer { approve, note }).await?;
                    continue;
                }
                match line.as_str() {
                    "" => { if !screen.busy { prompt(); } }
                    "/help" => { println!("{HELP}"); if !screen.busy { prompt(); } }
                    "/quit" => break DrainPoint::Turn,
                    "/pause" => break DrainPoint::Step,
                    "/cancel" => handle.cancel().await?,
                    "/kill" => {
                        println!("{DIM}Exiting without saving.{RESET}");
                        std::process::exit(0);
                    }
                    text if text.starts_with('/') => println!("Unknown command. /help lists commands."),
                    text if screen.busy => {
                        handle.send(Delivery::NextStep, text).await?;
                        println!("{DIM}(added to the agent's next step){RESET}");
                    }
                    text => {
                        handle.send(Delivery::NextTurn, text).await?;
                        screen.busy = true;
                    }
                }
            }
            observation = observations.recv() => match observation {
                Ok(o) => screen.show(o),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break DrainPoint::Turn,
            },
            _ = tokio::signal::ctrl_c() => {
                if screen.busy { handle.cancel().await?; } else { println!(); break DrainPoint::Turn; }
            }
            result = &mut agent.task => {
                println!("{RED}The agent stopped: {result:?}{RESET}");
                return Ok(());
            }
        }
    };

    if screen.busy && drain_at == DrainPoint::Turn {
        screen.line_break();
        println!("{DIM}Waiting for the current turn to finish…{RESET}");
    }
    // Keep showing the agent's output until it reaches the safe point.
    let drain = handle.drain(drain_at, DRAIN_DEADLINE);
    tokio::pin!(drain);
    let result = loop {
        tokio::select! {
            result = &mut drain => break result,
            Ok(observation) = observations.recv() => screen.show(observation),
        }
    };
    screen.line_break();
    match result? {
        DrainResult::Quiesced { image, forced } => {
            let path = save_image(images_dir, &image)?;
            let how = if forced {
                " (the turn was cut short at the deadline)"
            } else {
                ""
            };
            println!(
                "Saved `{}` to {}{how}. Run the same command to continue.",
                handle.id,
                path.display()
            );
        }
        DrainResult::Failed(e) => println!(
            "{RED}Could not save the agent: {e}. The next start recovers it from its log.{RESET}"
        ),
    }
    let _ = agent.task.await;
    Ok(())
}
