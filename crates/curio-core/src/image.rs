//! The agent image: a quiesced agent's whole context as data.

use crate::config::AgentConfig;
use crate::event::View;
use crate::ids::PluginId;
use crate::log::LogPosition;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the image encoding.
pub const IMAGE_FORMAT: u32 = 1;

/// A quiesced agent. Produced by a drain, consumed by thaw on this host or any other.
/// Plugin instances, grants and the queue consumer are not in it: thaw rebuilds them from
/// `config`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentImage {
    pub format: u32,
    pub config: AgentConfig,
    /// Every plugin's state slot, with its runner's state version.
    pub slots: Vec<SlotImage>,
    /// History, inbox, counters and any suspended turn.
    pub view: View,
    /// Where the log ended when the image was taken. Thaw refuses a log that moved past it.
    pub log_position: LogPosition,
}

/// One plugin's serialized state slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SlotImage {
    pub plugin: PluginId,
    pub version: u32,
    pub blob: Value,
}
