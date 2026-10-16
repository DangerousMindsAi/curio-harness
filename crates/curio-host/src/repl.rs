//! The terminal conversation with one agent.
//!
//! [`run`] is a small event loop over four sources at once: lines you type, the agent's
//! live [`Observation`]s, Ctrl-C, and the agent's task finishing on its own (which only
//! happens when something went wrong, such as the log being fenced by another host).
//! The REPL never touches the agent's state. It talks to the agent the same way any host
//! would, through its [`curio_core::AgentHandle`]: typed text is published to the agent's
//! message queue, and `/cancel`, approval answers and drains are commands.
//!
//! What you type is delivered in one of two ways. When the agent is idle, a line becomes
//! a [`Delivery::NextTurn`] message and starts a turn. While a turn is running, a line
//! becomes a [`Delivery::NextStep`] message, which the driver picks up at its next step
//! boundary, so you can steer the agent mid-turn without cancelling it.
//!
//! Leaving is a drain. `/quit` (or Ctrl-D, or Ctrl-C while idle) drains at the turn
//! boundary and `/pause` at the next step boundary; either way the REPL keeps printing
//! the agent's output until it reaches that safe point, then saves the image it hands
//! back with [`crate::save_image`]. `/kill` skips all of that and exits the process, which
//! leaves the log as a crash would and exercises recovery on the next start.

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

// ANSI escape codes for the little styling the REPL uses.
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// The `/help` text, which is also the list of commands [`run`] understands.
const HELP: &str = "Type a message and press Enter. While the agent is working, what you type is added to its next step.
  /quit    finish the current turn, save the agent and exit (Ctrl-D does the same)
  /pause   stop at the next step boundary, save the agent mid-turn and exit
  /cancel  cancel the current turn (Ctrl-C does the same while a turn runs)
  /kill    exit at once without saving, as a crash would; the next start recovers from the log
  /help    show this";

/// How long `/quit` and `/pause` wait for the safe point. After this the agent cancels
/// the running turn and quiesces anyway, and the save message says the stop was forced.
const DRAIN_DEADLINE: Duration = Duration::from_secs(120);

/// Prints the input prompt. Shown only while the agent is idle, so a prompt on screen
/// means "type a new message".
fn prompt() {
    print!("{CYAN}> {RESET}");
    let _ = std::io::stdout().flush();
}

/// Squeezes text onto one line and cuts it to `max` characters, for showing tool
/// arguments and results without flooding the terminal. The agent always sees the full text.
fn short(text: &str, max: usize) -> String {
    let one_line = text.replace('\n', " ⏎ ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    }
}

/// The REPL's view of the conversation: what the terminal is showing and what kind of
/// input it expects next.
#[derive(Default)]
struct Screen {
    /// A turn is running, so typed text is steering and Ctrl-C cancels.
    busy: bool,
    /// The agent asked to approve a tool call; the next typed line answers it.
    approval: Option<RequestId>,
    /// The cursor is in the middle of streamed text, so the next block needs a newline first.
    mid_line: bool,
}

impl Screen {
    /// Ends the current line if streamed text left the cursor mid-line.
    fn line_break(&mut self) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
    }

    /// Prints one observation and updates the screen state from it. Model text streams
    /// inline as it arrives, reasoning is dimmed, tool calls and results get one short
    /// line each, and an approval request prints a question and arms [`Screen::approval`].
    /// A turn's end clears the busy flag and shows the prompt again.
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

/// Runs the conversation with `agent` until you leave, then drains it and saves its image
/// to `images_dir`.
///
/// Returns early without saving when the agent's task stops by itself; the reason is
/// printed, and the next start recovers the agent from its log. A drain that fails (the
/// agent hit an error before reaching the safe point) is reported the same way.
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
                // While an approval is pending, the line is its answer: "y" approves, an
                // empty line or "n" refuses, and anything else refuses with that reason.
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
