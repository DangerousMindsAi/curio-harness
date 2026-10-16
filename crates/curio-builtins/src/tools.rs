//! Built-in tools: `clock`, `shell`, `read_file` and `write_file`.
//!
//! Each tool is a per-agent plugin that implements [`Tool`] and subscribes to no hooks.
//! The driver lists every tool's [`ToolSpec`] in each model request, and when the model
//! calls one, runs [`Tool::call`] with the parsed arguments and a [`ToolCx`] carrying the
//! call id, the plugin's grants and a cancel token. Tools are stateless: everything they
//! know comes from their code, their grants and the call. The name the model sees is fixed
//! per runner (`clock`, `shell`, `read_file`, `write_file`), so an agent can have at most one
//! plugin of each.
//!
//! The file and shell tools work in a workspace: the plugin's first `grants.fs` root,
//! resolved against the startup file's directory, created if missing, and canonicalized at
//! load. Their only code field is `max_output_bytes`, which caps the text a result puts in
//! front of the model (default 16000 bytes; the whole output is still read into memory
//! first). `clock` takes no code and no grants.
//!
//! ```yaml
//! - id: clock
//!   runner: builtin.tool.clock
//! - id: shell
//!   runner: builtin.tool.shell
//!   code: { max_output_bytes: 32000 }       # optional
//!   grants: { fs: [data/workspaces/assistant] }
//! - id: read_file
//!   runner: builtin.tool.read_file
//!   grants: { fs: [data/workspaces/assistant] }
//! ```
//!
//! `shell` only sets its working directory to the workspace: the command itself is not
//! sandboxed and can read, write and reach anything the host process can, so pair it with
//! `builtin.approval`. The file tools refuse absolute paths and `..`. `read_file` also
//! checks, after resolving symlinks, that the file it reads is inside the workspace, and
//! `write_file` refuses any path that passes through a symlink.
//!
//! `clock` and `read_file` are parallel-safe, so the driver may run several of them at once;
//! `shell` and `write_file` are exclusive and run alone. Every tool returns its failures as
//! error results the model reads, never as harness errors.

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

/// `builtin.tool.clock`: the current time. Its code must be empty or absent.
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

/// The clock tool: returns the host's current time in UTC, to the second, as RFC 3339.
pub struct Clock {
    /// The fixed spec: name `clock`, no parameters, parallel-safe.
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

    /// Ignores its arguments and never fails.
    async fn call(&self, _cx: ToolCx, _args: Value) -> ToolOutput {
        ToolOutput::ok(humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string())
    }
}

/// The code shared by the workspace tools (`shell`, `read_file`, `write_file`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceCode {
    /// The most bytes of output a result shows the model; the rest is replaced by a note
    /// saying how much was cut. Defaults to 16000. `write_file` accepts it but doesn't use
    /// it.
    #[serde(default = "default_max_output")]
    max_output_bytes: usize,
}

/// Default for [`WorkspaceCode::max_output_bytes`].
fn default_max_output() -> usize {
    16_000
}

/// Resolves and creates the plugin's workspace: its first `grants.fs` root.
///
/// Fails at load when the plugin has no `fs` grant or the directory can't be created.
/// Returns the canonical path, so later symlink checks compare like with like. Any further
/// `fs` roots are ignored.
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

/// The shell tool: runs `sh -c <command>` in the workspace with stdin closed and returns
/// the exit code, stdout and stderr in one text result.
///
/// Not sandboxed. Environment variables are inherited from the host process.
pub struct Shell {
    /// The fixed spec: name `shell`, one required `command` string, exclusive.
    spec: ToolSpec,
    /// The canonical workspace, used as the working directory.
    root: PathBuf,
    /// From [`WorkspaceCode::max_output_bytes`].
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

    /// Runs the command to completion and reports it. A non-zero exit, or death by a
    /// signal (shown as exit code `none (killed)`), makes the result an error.
    ///
    /// When `cx.cancel` fires, or the driver's tool timeout drops this future, the `sh`
    /// process is killed (`kill_on_drop`) and the result says the outcome is unknown.
    /// Only `sh` itself is killed: anything it started in the background can keep running.
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

/// `builtin.tool.read_file` and `builtin.tool.write_file`: one runner type registered
/// twice, once per direction.
pub struct FileToolRunner {
    /// Whether this is the `write_file` runner.
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

    /// Builds the spec for this direction and sets up the workspace. `read_file` is
    /// parallel-safe; `write_file` is exclusive, so two writes never race.
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

/// A file tool confined to the workspace: `read_file` or `write_file`.
pub struct FileTool {
    /// The spec built at load for this direction.
    spec: ToolSpec,
    /// The canonical workspace that every path is resolved against and checked to stay in.
    root: PathBuf,
    /// Whether this is `write_file`.
    write: bool,
    /// From [`WorkspaceCode::max_output_bytes`]; used by `read_file` only.
    max_output: usize,
}

impl Plugin for FileTool {
    fn as_tool(&self) -> Option<&dyn Tool> {
        Some(self)
    }
}

/// Refuses `rel` if any part of it that already exists below `root`, including the final
/// file name, is a symbolic link. `write_file` uses this before creating anything, so the
/// write cannot follow a link out of the workspace. A link swapped in between this check
/// and the write is not caught; the tool is confinement for a cooperative model, not a
/// sandbox against a hostile process sharing the workspace.
async fn refuse_symlinks(root: &Path, rel: &Path) -> std::result::Result<(), String> {
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        match tokio::fs::symlink_metadata(&current).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("`{}` is a symbolic link", current.display()));
            }
            Ok(_) => {}
            // Nothing below a missing component exists yet, so there is nothing to follow.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

/// Fails if `path`, after resolving symlinks, is outside `root`. Also fails, with the OS
/// error text, when `path` doesn't exist. `root` must already be canonical.
///
/// `read_file` uses this. The check and the later read are separate steps, so a symlink
/// swapped in between them isn't caught. With `shell` in the same workspace this doesn't matter,
/// since the shell isn't confined at all.
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

    /// Reads or writes one file, reporting every failure as an error result.
    ///
    /// `read_file` checks that the resolved file is inside the workspace, then returns its
    /// contents as lossy UTF-8, truncated to `max_output_bytes`. `write_file` creates the
    /// parent directories, checks that the resolved parent is inside the workspace, then
    /// replaces the file in one non-atomic write. Neither watches the cancel token; both
    /// finish quickly.
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
            // Checked before anything is created, so a symlinked directory inside the
            // workspace cannot be used to create directories or write a file outside it.
            if let Err(e) = refuse_symlinks(&self.root, Path::new(rel)).await {
                return ToolOutput::error(format!("Refused: {e}"));
            }
            let parent = path.parent().unwrap_or(&self.root).to_path_buf();
            if let Err(e) = tokio::fs::create_dir_all(&parent).await {
                return ToolOutput::error(format!("Could not create {rel}'s directory: {e}"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_file_refuses_to_follow_symlinks_out_of_the_workspace() {
        let outside = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = tokio::fs::canonicalize(root.path()).await.unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("dir")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("f"), root.join("file")).unwrap();
        assert!(
            refuse_symlinks(&root, Path::new("dir/x/y.txt"))
                .await
                .is_err()
        );
        assert!(refuse_symlinks(&root, Path::new("file")).await.is_err());
        assert!(refuse_symlinks(&root, Path::new("new/y.txt")).await.is_ok());
    }
}
