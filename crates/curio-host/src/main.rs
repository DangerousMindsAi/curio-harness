//! `curio`: a local curio-harness host. Reads a host YAML file (the runtime's startup
//! file), creates, thaws or recovers one agent, and lets you talk to it in the terminal.

mod repl;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use curio_core::{
    AgentId, AgentImage, CreateRequest, Error, RunningAgent, Runtime, RuntimeConfig,
    RuntimePluginSpec,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "curio", about = "Run curio-harness agents on this machine")]
struct Cli {
    /// The host configuration file.
    #[arg(long, short, default_value = "host.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

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

/// The host's startup file: the runtime configuration plus where images are kept.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    host_id: String,
    /// Where `/quit` saves agent images and where the next start looks for them.
    images_dir: PathBuf,
    runners: Vec<String>,
    log_store: RuntimePluginSpec,
    provisioner: RuntimePluginSpec,
    secret_store: RuntimePluginSpec,
}

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

/// Thaws the agent from its image if one was saved, recovers it from its log if it has
/// one, and creates it otherwise.
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
        let result = runtime.thaw(image).await;
        // The log has moved past the image now (or already had), so it can never be thawed again.
        std::fs::remove_file(&image_path)?;
        return match result {
            Ok(agent) => {
                println!("Resumed `{id}` from its saved image.");
                Ok(agent)
            }
            Err(Error::StaleImage { .. }) => {
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

fn image_path(images_dir: &Path, id: &AgentId) -> PathBuf {
    images_dir.join(format!("{id}.json"))
}

/// Writes an image so that a crash mid-write never leaves a torn file behind.
fn save_image(images_dir: &Path, image: &AgentImage) -> Result<PathBuf> {
    std::fs::create_dir_all(images_dir)?;
    let path = image_path(images_dir, &image.config.id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(image)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}
