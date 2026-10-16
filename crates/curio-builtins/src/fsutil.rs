//! Small filesystem helpers shared by the file-backed runners.
//!
//! Nothing here is a plugin. The file log store, the file queue and the provisioner use
//! [`path_component`] to turn agent ids and template names into directory and file names,
//! and [`write_atomic`] to replace small state files (epochs, acknowledged ids, the
//! compacted message file) without leaving a half-written file behind. The workspace tools
//! use [`join_inside`] to keep model-supplied paths under their workspace root, and
//! [`truncate`] to cap what a tool result puts in front of the model.
//!
//! These checks are lexical. [`join_inside`] doesn't resolve symlinks; the file tools in
//! [`crate::tools`] do that separately after joining.

use curio_core::{Error, Result};
use std::path::{Component, Path, PathBuf};

/// Checks that `id` can be used as exactly one path component, and returns it unchanged.
///
/// Rejects the empty string, `.` and `..`, and anything containing a slash, a backslash or
/// a NUL byte, so an agent id or template name can never climb out of the directory it's
/// joined onto. Fails with a configuration error naming the offending id.
pub fn path_component(id: &str) -> Result<&str> {
    let ok = !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains(['/', '\\', '\0'])
        && Path::new(id).components().count() == 1;
    if ok {
        Ok(id)
    } else {
        Err(Error::config(format!(
            "`{id}` cannot be used as a file name"
        )))
    }
}

/// Replaces the file at `path` with `contents` so a reader sees either the old file or the
/// new one, never a mix.
///
/// Writes a sibling with the extension replaced by `tmp` (so `epoch` becomes `epoch.tmp`
/// and `messages.jsonl` becomes `messages.tmp`), syncs it to disk, then renames it over the
/// target. The rename is atomic on POSIX filesystems. The parent directory isn't synced, so
/// a power loss right after the rename can still leave the old file in place. Two callers
/// writing the same path at once share the temporary name, so callers serialize their own
/// writes.
pub async fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Joins a model-supplied relative path onto `root`, refusing absolute paths and any `..`
/// (or root or prefix) component.
///
/// The error is a sentence meant for the model, which is why it's a `String` instead of an
/// [`Error`]. The check is purely lexical: a symlink inside `root` can still point outside
/// it, so callers that care resolve the result and compare it with `root` themselves.
pub fn join_inside(root: &Path, relative: &str) -> std::result::Result<PathBuf, String> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err(format!(
            "`{relative}` is absolute; use a path relative to the workspace"
        ));
    }
    if rel
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!("`{relative}` leaves the workspace"));
    }
    Ok(root.join(rel))
}

/// Cuts `text` to at most `max` bytes, backing up to the nearest character boundary, and
/// appends a line saying how many bytes were dropped.
///
/// Text that already fits comes back unchanged. The appended note isn't counted against
/// `max`, so the result can be a few dozen bytes longer than `max`.
pub fn truncate(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... {} more bytes not shown]",
        &text[..end],
        text.len() - end
    )
}
