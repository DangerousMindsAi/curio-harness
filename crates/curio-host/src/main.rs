//! `curio`, the local curio-harness host.
//!
//! A host is the process that owns a [`curio_core::Runtime`] and decides which agents run
//! in it. In production that would be a server taking orders from an orchestrator; this
//! one is a command-line program for one person on one machine. It reads the host YAML
//! file (the runtime's startup file in the proposal's terms), starts a runtime with every
//! built-in runner from [`curio_builtins::registry`], brings one agent to life and hands
//! it to [`repl`] so you can talk to it in the terminal.
//!
//! Bringing an agent to life uses whichever of the runtime's three entry points fits what
//! is on disk, checked in this order by [`load_agent`]:
//!
//! 1. A saved image in `images_dir` means the agent was quiesced cleanly last time, so it
//!    is thawed ([`curio_core::Runtime::thaw`]) and continues exactly where it stopped.
//! 2. A session log without an image means the process stopped without quiescing (a crash,
//!    a kill, or `/kill`), so it is recovered from the log ([`curio_core::Runtime::recover`]).
//! 3. Neither means the agent is new, so the provisioner creates it from its template
//!    ([`curio_core::Runtime::create`]).
//!
//! Relative paths in the host file and in agent files resolve against the directory that
//! holds the host file, so the program behaves the same from any working directory.

mod repl;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use curio_core::{
    AgentId, AgentImage, CreateRequest, Error, RunningAgent, Runtime, RuntimeConfig,
    RuntimePluginSpec,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Command-line arguments.
#[derive(Parser)]
#[command(name = "curio", about = "Run curio-harness agents on this machine")]
struct Cli {
    /// The host configuration file.
    #[arg(long, short, default_value = "host.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

/// What to do once the runtime is up. There is one subcommand for now; more (listing
/// agents, sending a message without a terminal session) would go here.
#[derive(Subcommand)]
enum Command {
    /// Talk to an agent. Creates it from `<template>.yaml` in the provisioner's directory the
    /// first time; afterwards resumes it from its saved image, or recovers it from its log.
    Chat {
        /// The agent template, which is also the agent id unless `--id` is given.
        template: String,
        /// Run a separate agent from the same template.
        #[arg(long)]
        id: Option<String>,
    },
}

/// The host's startup file, as read from YAML. Everything except `images_dir` is passed
/// straight to the runtime as a [`RuntimeConfig`]; `images_dir` belongs to the host
/// because what happens to an image after a drain is outside the harness. Unknown keys
/// are rejected so a typo fails at start instead of being ignored.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    /// Names this host in the `Thawed` and `Recovered` events it writes to agent logs.
    host_id: String,
    /// Where `/quit` and `/pause` save agent images and where the next start looks for them.
    images_dir: PathBuf,
    /// Runner names agents may use; see [`RuntimeConfig::runners`].
    runners: Vec<String>,
    /// The runtime-level plugin that keeps every agent's session log.
    log_store: RuntimePluginSpec,
    /// The runtime-level plugin that produces a new agent's configuration from a template.
    provisioner: RuntimePluginSpec,
    /// The runtime-level plugin that resolves secret names, such as API keys.
    secret_store: RuntimePluginSpec,
}

/// Reads the host file, starts the runtime and runs the subcommand. Any configuration
/// problem (an unreadable file, an unknown key, a runner that fails to load) ends the
/// program here with a message, before an agent is touched.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let text = std::fs::read_to_string(&cli.config).with_context(|| {
        format!(
            "cannot read {} (copy examples/host.example.yaml to start)",
            cli.config.display()
        )
    })?;
    let host: HostConfig = serde_yaml_ng::from_str(&text)
        .with_context(|| format!("invalid host config {}", cli.config.display()))?;
    let base_dir = std::fs::canonicalize(&cli.config)?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let images_dir = base_dir.join(&host.images_dir);
    let runtime_config = RuntimeConfig {
        host_id: host.host_id,
        runners: host.runners,
        log_store: host.log_store,
        provisioner: host.provisioner,
        secret_store: host.secret_store,
    };
    let runtime = Runtime::start(runtime_config, curio_builtins::registry()?, base_dir).await?;

    match cli.command {
        Command::Chat { template, id } => {
            let id = AgentId::new(id.unwrap_or_else(|| template.clone()));
            let agent = load_agent(&runtime, &images_dir, &template, &id).await?;
            repl::run(agent, &images_dir).await
        }
    }
}

/// Starts agent `id`: thaws it from its saved image if there is one, recovers it from its
/// log if it has one, and otherwise creates it from `template`. Prints one line saying
/// which happened.
///
/// After a successful thaw the image file is deleted: the thaw appended a `Thawed` event,
/// so the log has moved past the image and the runtime would refuse it next time anyway.
/// A stale image (the log moved on while the image sat on disk) is deleted too, and the
/// agent is recovered from its log instead. Any other thaw failure, such as a plugin that
/// no longer loads, keeps the image so that fixing the configuration and starting again
/// still resumes from it.
async fn load_agent(
    runtime: &Runtime,
    images_dir: &Path,
    template: &str,
    id: &AgentId,
) -> Result<RunningAgent> {
    let image_path = image_path(images_dir, id);
    if image_path.exists() {
        let image: AgentImage = serde_json::from_slice(&std::fs::read(&image_path)?)
            .with_context(|| format!("unreadable image {}", image_path.display()))?;
        if image.config.id != *id {
            bail!(
                "{} holds agent `{}`, not `{id}`",
                image_path.display(),
                image.config.id
            );
        }
        return match runtime.thaw(image).await {
            Ok(agent) => {
                std::fs::remove_file(&image_path)?;
                println!("Resumed `{id}` from its saved image.");
                Ok(agent)
            }
            Err(Error::StaleImage { .. }) => {
                std::fs::remove_file(&image_path)?;
                println!(
                    "The saved image of `{id}` is out of date; recovering from its log instead."
                );
                Ok(runtime.recover(id).await?)
            }
            Err(e) => Err(e.into()),
        };
    }
    if runtime.log_store().exists(id).await? {
        println!("`{id}` did not shut down cleanly last time; recovering it from its log.");
        return Ok(runtime.recover(id).await?);
    }
    let agent = runtime
        .create(CreateRequest {
            template: template.to_string(),
            agent_id: Some(id.clone()),
            params: serde_json::Value::Null,
        })
        .await?;
    println!("Created `{id}` from template `{template}`.");
    Ok(agent)
}

/// Where the image of agent `id` is saved: `<images_dir>/<id>.json`.
fn image_path(images_dir: &Path, id: &AgentId) -> PathBuf {
    images_dir.join(format!("{id}.json"))
}

/// Saves an image as pretty-printed JSON and returns its path. Writes a temporary file and
/// renames it into place, so a crash mid-write leaves either the old image or the new one,
/// never a torn file that the next start would fail to read.
fn save_image(images_dir: &Path, image: &AgentImage) -> Result<PathBuf> {
    std::fs::create_dir_all(images_dir)?;
    let path = image_path(images_dir, &image.config.id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(image)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}
