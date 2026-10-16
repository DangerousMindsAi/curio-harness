//! The agent image: a quiesced agent's whole context as data.
//!
//! An [`AgentImage`] is what a drain hands back once the agent reaches a safe point (see
//! [`crate::AgentCtx::settle_drain`]), and what [`crate::Runtime::thaw`] takes to bring the
//! agent back, on this host or any other. How it travels between the two is the caller's
//! business; the local host writes it to a JSON file.
//!
//! The image holds only data. Plugin instances, the queue consumer and the open log are
//! rebuilt on thaw from `config`, by the same loading path that creates an agent, and each
//! plugin's state comes back from its [`SlotImage`]. The [`crate::View`] is copied whole, so
//! thaw doesn't read or replay the log. The image and the log must agree: thaw opens the log
//! with [`crate::OpenMode::ContinueFrom`] at `log_position` and refuses with
//! [`crate::Error::StaleImage`] if the log has grown since, so an old image can't overwrite
//! newer history.

use crate::config::AgentConfig;
use crate::event::View;
use crate::ids::PluginId;
use crate::log::LogPosition;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the image encoding. Thaw refuses any other value.
pub const IMAGE_FORMAT: u32 = 1;

/// A quiesced agent. Produced by a drain, consumed by thaw on this host or any other.
/// Plugin instances, grants and the queue consumer are not in it: thaw rebuilds them from
/// `config`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentImage {
    /// [`IMAGE_FORMAT`] at the time the image was taken.
    pub format: u32,
    /// The agent's pinned configuration, as in its log header.
    pub config: AgentConfig,
    /// Every plugin's state slot, with its runner's state version. Thaw requires exactly one
    /// slot per plugin in `config`.
    pub slots: Vec<SlotImage>,
    /// History, inbox, counters and any suspended turn.
    pub view: View,
    /// Where the log ended when the image was taken. Thaw refuses a log that moved past it.
    pub log_position: LogPosition,
}

/// One plugin's serialized state slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotImage {
    /// The plugin the slot belongs to.
    pub plugin: PluginId,
    /// The runner's [`crate::Runner::state_version`] when the slot was written. On thaw, an
    /// older version goes through [`crate::Runner::migrate`] and a newer one is refused.
    pub version: u32,
    /// The slot as [`crate::PluginState::to_blob`] wrote it; decoded by
    /// [`crate::Plugin::decode_state`].
    pub blob: Value,
}
