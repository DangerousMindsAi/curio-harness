//! `builtin.secrets.inline`: secrets listed in the host's startup file, keyed by
//! `<tenant>/<name>`.
//!
//! This is a runtime-level plugin: the startup file's `secret_store` entry names it, and
//! [`curio_core::Runtime::start`] loads it first, before the log store and provisioner, so
//! they can resolve secrets too. It implements [`SecretStore`] and subscribes to no hooks.
//!
//! Plugins never talk to the store directly. Each plugin gets a [`curio_core::SecretScope`]
//! limited to its own tenant and its `grants.secrets` list; the scope prefixes the tenant
//! and calls [`SecretStore::get`]. An agent in tenant `acme` that asks for `model-key`
//! therefore reads `acme/model-key` and can't name another tenant's secret at all.
//! Runtime-level plugins resolve under the tenant `host`.
//!
//! Each value is either the secret itself or `{ env: VAR }`, which reads the environment
//! variable on every lookup, so a rotated variable takes effect without a restart:
//!
//! ```yaml
//! secret_store:
//!   runner: builtin.secrets.inline
//!   code:
//!     secrets:
//!       local/deepinfra: "sk-..."               # the value itself
//!       acme/model-key: { env: ACME_MODEL_KEY } # read from the environment per lookup
//! ```
//!
//! Literal values sit in the startup file in plain text. That's fine for a local host with
//! a gitignored file and nothing more; a shared deployment wants a store backed by a real
//! secret manager.

use async_trait::async_trait;
use curio_core::{Error, LoadEnv, LoadedPlugin, Plugin, Result, Runner, SecretStore, TenantId};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Loads [`InlineSecrets`] from the `secrets` table. Holds nothing itself.
pub struct InlineSecretsRunner;

/// The plugin's code: one table of secrets.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretsCode {
    /// Secrets by `<tenant>/<name>`. Both parts must be non-empty and the key must hold
    /// exactly one slash. Defaults to empty, which makes every lookup miss.
    #[serde(default)]
    secrets: HashMap<String, SecretValue>,
}

/// One secret's value as written in the startup file. Untagged, so a plain string is a
/// literal and a `{ env: ... }` table is a variable reference.
#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum SecretValue {
    /// The secret itself.
    Literal(String),
    /// The name of an environment variable holding the secret. Read at each lookup; an
    /// unset or empty variable counts as a missing secret.
    Env { env: String },
}

#[async_trait]
impl Runner for InlineSecretsRunner {
    fn name(&self) -> &str {
        "builtin.secrets.inline"
    }

    /// Parses the table and checks every key's `<tenant>/<name>` form. Environment
    /// variables aren't read here, so a variable that's unset at startup only fails when a
    /// plugin resolves it (for the model plugin, that's at its own load).
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin> {
        let code: SecretsCode = env.parse_code(code)?;
        for key in code.secrets.keys() {
            if key.split('/').count() != 2 || key.split('/').any(str::is_empty) {
                return Err(Error::plugin(
                    &env.plugin,
                    format!("secret key `{key}` is not `<tenant>/<name>`"),
                ));
            }
        }
        Ok(LoadedPlugin::stateless(Arc::new(InlineSecrets {
            secrets: code.secrets,
        })))
    }
}

/// Secrets from the startup file, held in memory for the life of the process. Immutable
/// after load; there's no way to add or change a secret without restarting the host.
pub struct InlineSecrets {
    /// The parsed `secrets` table, by `<tenant>/<name>`.
    secrets: HashMap<String, SecretValue>,
}

impl Plugin for InlineSecrets {
    fn as_secret_store(&self) -> Option<&dyn SecretStore> {
        Some(self)
    }
}

#[async_trait]
impl SecretStore for InlineSecrets {
    /// Looks up `<tenant>/<name>`. Returns `Ok(None)` when the key isn't listed or its
    /// environment variable is unset or empty; never fails.
    async fn get(&self, tenant: &TenantId, name: &str) -> Result<Option<String>> {
        Ok(match self.secrets.get(&format!("{tenant}/{name}")) {
            None => None,
            Some(SecretValue::Literal(v)) => Some(v.clone()),
            Some(SecretValue::Env { env }) => std::env::var(env).ok().filter(|v| !v.is_empty()),
        })
    }
}
