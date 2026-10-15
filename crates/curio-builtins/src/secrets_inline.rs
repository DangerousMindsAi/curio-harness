//! `builtin.secrets.inline`: secrets listed in the host's startup file, by
//! `<tenant>/<name>`. Each value is either the secret itself or `{ env: VAR }`.

use async_trait::async_trait;
use curio_core::{Error, LoadEnv, LoadedPlugin, Plugin, Result, Runner, SecretStore, TenantId};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Loads [`InlineSecrets`].
pub struct InlineSecretsRunner;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretsCode {
    #[serde(default)]
    secrets: HashMap<String, SecretValue>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum SecretValue {
    Literal(String),
    Env { env: String },
}

#[async_trait]
impl Runner for InlineSecretsRunner {
    fn name(&self) -> &str {
        "builtin.secrets.inline"
    }

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

/// Secrets from the startup file.
pub struct InlineSecrets {
    secrets: HashMap<String, SecretValue>,
}

impl Plugin for InlineSecrets {
    fn as_secret_store(&self) -> Option<&dyn SecretStore> {
        Some(self)
    }
}

#[async_trait]
impl SecretStore for InlineSecrets {
    async fn get(&self, tenant: &TenantId, name: &str) -> Result<Option<String>> {
        Ok(match self.secrets.get(&format!("{tenant}/{name}")) {
            None => None,
            Some(SecretValue::Literal(v)) => Some(v.clone()),
            Some(SecretValue::Env { env }) => std::env::var(env).ok().filter(|v| !v.is_empty()),
        })
    }
}
