//! Built-in tools: `clock`, `shell`, `read_file` and `write_file`.
//!
//! The file and shell tools work in the plugin's first `grants.fs` root. `shell` only sets
//! its working directory there: the command itself is not sandboxed, so pair it with
//! `builtin.approval`.

use crate::fsutil::{join_inside, truncate};
use async_trait::async_trait;
use curio_core::{
    Concurrency, Error, LoadEnv, LoadedPlugin, Plugin, Result, Runner, Tool, ToolCx, ToolOutput,
    ToolSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

/// `builtin.tool.clock`: the current time.
pub struct ClockRunner;

#[async_trait]
impl Runner for ClockRunner {
    fn name(&self) -> &str {
        "builtin.tool.clock"
    }

    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct NoCode {}
        env.parse_code::<NoCode>(code)?;
        let spec = ToolSpec {
            name: "clock".into(),
            description: "Returns the current date and time in UTC (RFC 3339).".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            concurrency: Concurrency::Parallel,
        };
        Ok(LoadedPlugin::stateless(Arc::new(Clock { spec })))
    }
}

/// The clock tool.
pub struct Clock {
    spec: ToolSpec,
}

impl Plugin for Clock {
    fn as_tool(&self) -> Option<&dyn Tool> {
        Some(self)
    }
}

#[async_trait]
impl Tool for Clock {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, _cx: ToolCx, _args: Value) -> ToolOutput {
        ToolOutput::ok(humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCode {
    #[serde(default = "default_max_output")]
    max_output_bytes: usize,
}

fn default_max_output() -> usize {
    16_000
}

/// Resolves and creates the plugin's workspace: its first `grants.fs` root.
async fn workspace_root(env: &LoadEnv) -> Result<PathBuf> {
    let root =
        env.grants.fs.first().ok_or_else(|| {
            Error::plugin(&env.plugin, "this tool needs a workspace in grants.fs")
        })?;
    let root = env.resolve_path(root);
    tokio::fs::create_dir_all(&root).await?;
    Ok(tokio::fs::canonicalize(&root).await?)
}

/// `builtin.tool.shell`: runs a shell command in the workspace.
pub struct ShellRunner;

#[async_trait]
impl Runner for ShellRunner {
    fn name(&self) -> &str {
        "builtin.tool.shell"
    }

    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: WorkspaceCode = env.parse_code(code)?;
        let spec = ToolSpec {
            name: "shell".into(),
            description:
                "Runs a command with `sh -c` in the workspace directory and returns its exit code, \
                          stdout and stderr."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The command line to run." } },
                "required": ["command"]
            }),
            concurrency: Concurrency::Exclusive,
        };
        let root = workspace_root(env).await?;
        Ok(LoadedPlugin::stateless(Arc::new(Shell {
            spec,
            root,
            max_output: code.max_output_bytes,
        })))
    }
}

/// The shell tool.
pub struct Shell {
    spec: ToolSpec,
    root: PathBuf,
    max_output: usize,
}

impl Plugin for Shell {
    fn as_tool(&self) -> Option<&dyn Tool> {
        Some(self)
    }
}

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, cx: ToolCx, args: Value) -> ToolOutput {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return ToolOutput::error("`command` must be a string.");
        };
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::select! {
            o = child => o,
            _ = cx.cancel.cancelled() => return ToolOutput::error("Cancelled while running; the outcome is unknown."),
        };
        match output {
            Err(e) => ToolOutput::error(format!("Could not start the command: {e}")),
            Ok(o) => {
                let code = o
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "none (killed)".into());
                let text = format!(
                    "exit code: {code}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                ToolOutput {
                    content: truncate(text, self.max_output),
                    is_error: !o.status.success(),
                }
            }
        }
    }
}

/// `builtin.tool.read_file` and `builtin.tool.write_file`.
pub struct FileToolRunner {
    write: bool,
}

impl FileToolRunner {
    /// The runner for `read_file`.
    pub fn read() -> Self {
        Self { write: false }
    }

    /// The runner for `write_file`.
    pub fn write() -> Self {
        Self { write: true }
    }
}

#[async_trait]
impl Runner for FileToolRunner {
    fn name(&self) -> &str {
        if self.write {
            "builtin.tool.write_file"
        } else {
            "builtin.tool.read_file"
        }
    }

    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: WorkspaceCode = env.parse_code(code)?;
        let spec = if self.write {
            ToolSpec {
                name: "write_file".into(),
                description:
                    "Writes text to a file in the workspace, creating directories as needed and \
                              replacing any existing file."
                        .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path relative to the workspace." },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
                concurrency: Concurrency::Exclusive,
            }
        } else {
            ToolSpec {
                name: "read_file".into(),
                description: "Reads a text file from the workspace.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string", "description": "Path relative to the workspace." } },
                    "required": ["path"]
                }),
                concurrency: Concurrency::Parallel,
            }
        };
        let root = workspace_root(env).await?;
        Ok(LoadedPlugin::stateless(Arc::new(FileTool {
            spec,
            root,
            write: self.write,
            max_output: code.max_output_bytes,
        })))
    }
}

/// A file tool confined to the workspace.
pub struct FileTool {
    spec: ToolSpec,
    root: PathBuf,
    write: bool,
    max_output: usize,
}

impl Plugin for FileTool {
    fn as_tool(&self) -> Option<&dyn Tool> {
        Some(self)
    }
}

/// Fails if `path`, after resolving symlinks, is outside `root`.
async fn check_inside(root: &Path, path: &Path) -> std::result::Result<(), String> {
    match tokio::fs::canonicalize(path).await {
        Ok(real) if real.starts_with(root) => Ok(()),
        Ok(_) => Err("the path resolves outside the workspace".into()),
        Err(e) => Err(e.to_string()),
    }
}

#[async_trait]
impl Tool for FileTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, _cx: ToolCx, args: Value) -> ToolOutput {
        let Some(rel) = args.get("path").and_then(Value::as_str) else {
            return ToolOutput::error("`path` must be a string.");
        };
        let path = match join_inside(&self.root, rel) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        if self.write {
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return ToolOutput::error("`content` must be a string.");
            };
            let parent = path.parent().unwrap_or(&self.root).to_path_buf();
            if let Err(e) = tokio::fs::create_dir_all(&parent).await {
                return ToolOutput::error(format!("Could not create {rel}'s directory: {e}"));
            }
            if let Err(e) = check_inside(&self.root, &parent).await {
                return ToolOutput::error(format!("Refused: {e}"));
            }
            match tokio::fs::write(&path, content).await {
                Ok(()) => ToolOutput::ok(format!("Wrote {} bytes to {rel}.", content.len())),
                Err(e) => ToolOutput::error(format!("Could not write {rel}: {e}")),
            }
        } else {
            if let Err(e) = check_inside(&self.root, &path).await {
                return ToolOutput::error(format!("Cannot read {rel}: {e}"));
            }
            match tokio::fs::read(&path).await {
                Ok(bytes) => ToolOutput::ok(truncate(
                    String::from_utf8_lossy(&bytes).into_owned(),
                    self.max_output,
                )),
                Err(e) => ToolOutput::error(format!("Cannot read {rel}: {e}")),
            }
        }
    }
}
