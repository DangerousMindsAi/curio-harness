//! Small filesystem helpers shared by the file-backed runners.

use curio_core::{Error, Result};
use std::path::{Component, Path, PathBuf};

/// Checks that an id can be used as one path component.
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

/// Writes a file by writing a sibling temporary file and renaming it over the target.
pub async fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Joins a model-supplied relative path onto `root`, refusing absolute paths and `..`.
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

/// Cuts text to at most `max` bytes on a character boundary, noting how much was dropped.
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
