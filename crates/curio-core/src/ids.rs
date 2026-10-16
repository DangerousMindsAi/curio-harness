//! Identifier newtypes. Each kind of id is its own type so they can't be mixed up.
//!
//! There are two families. String ids ([`AgentId`], [`TenantId`], [`PluginId`], [`CallId`],
//! [`RequestId`]) are chosen by someone outside the counter machinery: a provisioner, an
//! agent config, the model, or a plugin that asks a question. Number ids ([`TurnId`],
//! [`QueueMsgId`], [`InboxItemId`]) are assigned by a counter and increase, which the code
//! relies on: the queue's ids order messages and let a restarted agent skip redeliveries
//! (see [`crate::event::Inbox::last_logged`]), and turn and inbox item ids come from
//! counters in the folded [`crate::View`].
//!
//! Every id serializes as its bare inner value (`#[serde(transparent)]`), so logs, images
//! and queue files hold plain strings and numbers.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Defines a string-backed id newtype with `new`, `as_str` and `Display`.
macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps a string as this id. No validation happens here; any string is accepted.
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            /// The id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

/// Defines a `u64`-backed id newtype that is `Copy`, ordered and `Display`.
macro_rules! number_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

string_id!(
    /// One agent. Unique across every host that shares a log store, since the log store
    /// finds an agent's log by this id. Chosen by the provisioner at creation.
    AgentId
);
string_id!(
    /// The tenant an agent belongs to. Secrets are scoped by tenant: a plugin can only
    /// resolve secrets of its own agent's tenant (see [`crate::SecretScope`]).
    TenantId
);
string_id!(
    /// One plugin within one agent's configuration, as written in its
    /// [`crate::PluginSpec`]. Unique within the agent; checked by [`crate::PluginTable::build`].
    PluginId
);
string_id!(
    /// One tool call, as assigned by the model. Ties a call to its `ToolStarted` and
    /// `ToolResult` events, and tools may use it as an idempotency key.
    CallId
);
string_id!(
    /// One question a plugin asked outside the agent, such as an approval. The asker picks
    /// the id; the answer comes back in [`crate::Command::Answer`] under the same id.
    RequestId
);
number_id!(
    /// One turn of one agent, increasing from 1. The next id comes from
    /// [`crate::View::next_turn_id`].
    TurnId
);
number_id!(
    /// One message in one agent's queue, assigned by the queue and increasing. The agent
    /// relies on that order to skip messages it already logged.
    QueueMsgId
);
number_id!(
    /// One item in an agent's inbox, assigned when the item is folded into the view. Turn
    /// and step start events name the items they admit by this id.
    InboxItemId
);
