//! Identifier newtypes. Each kind of id is its own type so they cannot be mixed up.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps a string as this id.
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
    /// One agent. Unique across every host that shares a log store.
    AgentId
);
string_id!(
    /// The tenant an agent belongs to. Secrets and quotas are scoped by tenant.
    TenantId
);
string_id!(
    /// One plugin within one agent's configuration.
    PluginId
);
string_id!(
    /// One tool call, as assigned by the model.
    CallId
);
string_id!(
    /// One question a plugin asked outside the agent, such as an approval.
    RequestId
);
number_id!(
    /// One turn of one agent, increasing from 1.
    TurnId
);
number_id!(
    /// One message in one agent's queue, assigned by the queue and increasing.
    QueueMsgId
);
number_id!(
    /// One item in an agent's inbox, assigned when the item is logged.
    InboxItemId
);
