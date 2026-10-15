//! Runners, plugins, state slots and the service interfaces plugins provide.

use crate::agent::{AgentCtx, Exit, Observer};
use crate::config::{AgentConfig, PluginGrants};
use crate::error::{Error, Result};
use crate::event::Event;
use crate::hook::{Flow, HookCx, HookEvent, HookName, Subscription};
use crate::ids::{AgentId, CallId, PluginId, TenantId};
use crate::log::LogStore;
use crate::message::{AssistantMessage, ModelError, ModelRequest, ToolOutput, ToolSpec};
use crate::queue::MessageQueue;
use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Rust code compiled into the host, registered once per process under its name, and
/// shared by every agent. Holds only immutable, tenant-neutral resources.
#[async_trait]
pub trait Runner: Send + Sync + 'static {
    /// The runner name agent configs refer to, such as `builtin.tool.shell`.
    fn name(&self) -> &str;

    /// Version of the state slot format this runner writes. Bumped on incompatible change.
    fn state_version(&self) -> u32 {
        1
    }

    /// Validates and loads `code` for one plugin. Misconfiguration fails here, at create,
    /// thaw or recovery, never later in the loop.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin>;

    /// Upgrades a state slot written by an older version of this runner.
    fn migrate(&self, _blob: Value, from: u32) -> Result<Value> {
        Err(Error::config(format!(
            "runner `{}` cannot migrate state from version {from} to {}",
            self.name(),
            self.state_version()
        )))
    }
}

/// What a runner gets besides the code: who the plugin is for and what it may reach.
#[derive(Clone)]
pub struct LoadEnv {
    pub plugin: PluginId,
    /// `None` for runtime-level plugins.
    pub agent: Option<AgentId>,
    pub tenant: TenantId,
    pub grants: PluginGrants,
    /// Relative paths in code resolve against this directory (the host config's directory).
    pub base_dir: PathBuf,
    pub secrets: SecretScope,
}

impl LoadEnv {
    /// Resolves a path from code against the base directory.
    pub fn resolve_path(&self, path: impl AsRef<std::path::Path>) -> PathBuf {
        self.base_dir.join(path)
    }

    /// Fails unless `host` is in `grants.hosts`.
    pub fn require_host(&self, host: &str) -> Result<()> {
        if self.grants.hosts.iter().any(|h| h == host) {
            Ok(())
        } else {
            Err(Error::plugin(
                &self.plugin,
                format!("host `{host}` is not in grants.hosts"),
            ))
        }
    }

    /// Parses the code as the runner's configuration type.
    pub fn parse_code<T: DeserializeOwned>(&self, code: &Value) -> Result<T> {
        let code = if code.is_null() {
            Value::Object(Default::default())
        } else {
            code.clone()
        };
        serde_json::from_value(code)
            .map_err(|e| Error::plugin(&self.plugin, format!("invalid code: {e}")))
    }
}

/// What [`Runner::load`] returns.
pub struct LoadedPlugin {
    pub instance: Arc<dyn Plugin>,
    pub initial_state: Box<dyn PluginState>,
}

impl LoadedPlugin {
    /// A plugin whose state slot is empty.
    pub fn stateless(instance: Arc<dyn Plugin>) -> Self {
        Self {
            instance,
            initial_state: Box::new(()),
        }
    }
}

/// The mutable, serializable state of one plugin instance. Stored in the agent's context,
/// carried in images.
pub trait PluginState: Any + Send + Sync {
    /// Serializes the slot for an image.
    fn to_blob(&self) -> Result<Value>;
    /// For downcasting to the runner's state type.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Serialize + Any + Send + Sync> PluginState for T {
    fn to_blob(&self) -> Result<Value> {
        Ok(serde_json::to_value(self)?)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Decodes a state slot blob as `T`; for [`Plugin::decode_state`] implementations.
pub fn decode_state<T: DeserializeOwned + Serialize + Any + Send + Sync>(
    blob: Value,
) -> Result<Box<dyn PluginState>> {
    Ok(Box::new(serde_json::from_value::<T>(blob)?))
}

/// One plugin for one agent. Immutable after load: hooks take `&self`, and mutable state
/// lives in the slot [`HookCx::state`] exposes.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Hooks this plugin listens to.
    fn subscriptions(&self) -> Vec<Subscription> {
        Vec::new()
    }

    /// Handles one hook dispatch.
    async fn on_hook(&self, _cx: &mut HookCx<'_>, _event: &mut HookEvent<'_>) -> Result<Flow> {
        Ok(Flow::Continue)
    }

    /// Decodes a slot blob from an image. The default expects an empty slot.
    fn decode_state(&self, blob: Value) -> Result<Box<dyn PluginState>> {
        decode_state::<()>(blob)
    }

    fn as_driver(&self) -> Option<&dyn Driver> {
        None
    }
    fn as_model(&self) -> Option<&dyn ModelProvider> {
        None
    }
    fn as_tool(&self) -> Option<&dyn Tool> {
        None
    }
    fn as_queue(&self) -> Option<&dyn MessageQueue> {
        None
    }
    fn as_log_store(&self) -> Option<&dyn LogStore> {
        None
    }
    fn as_provisioner(&self) -> Option<&dyn Provisioner> {
        None
    }
    fn as_secret_store(&self) -> Option<&dyn SecretStore> {
        None
    }
}

/// The loop. Replacing the driver plugin replaces the loop.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Runs the agent until it quiesces or closes.
    async fn run(&self, cx: &mut AgentCtx) -> Result<Exit>;
}

/// A model provider. Streams partial output to `observer`; returns the complete reply.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Runs one attempt. Returns [`crate::ModelErrorKind::Cancelled`] soon after `cancel` fires.
    async fn complete(
        &self,
        request: ModelRequest,
        observer: Observer,
        cancel: CancellationToken,
    ) -> std::result::Result<AssistantMessage, ModelError>;
}

/// What a tool body gets: read-only facts about the call and the plugin's grants.
#[derive(Clone)]
pub struct ToolCx {
    pub agent: AgentId,
    pub tenant: TenantId,
    pub call_id: CallId,
    pub grants: PluginGrants,
    /// Cancelled when the turn is cancelled or a drain is forced.
    pub cancel: CancellationToken,
}

/// A tool the model can call.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    /// Runs the tool body. Must return soon after `cx.cancel` fires.
    async fn call(&self, cx: ToolCx, args: Value) -> ToolOutput;
}

/// Runtime-level service: resolves a tenant's secret names to values.
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, tenant: &TenantId, name: &str) -> Result<Option<String>>;
}

/// The secrets one plugin may resolve: its tenant's, limited to `grants.secrets`.
#[derive(Clone)]
pub struct SecretScope {
    store: Option<Arc<dyn Plugin>>,
    tenant: TenantId,
    allowed: Vec<String>,
}

impl SecretScope {
    /// A scope over `store` for one tenant and a list of granted names.
    pub fn new(store: Option<Arc<dyn Plugin>>, tenant: TenantId, allowed: Vec<String>) -> Self {
        Self {
            store,
            tenant,
            allowed,
        }
    }

    /// Resolves a granted secret. Call this per use rather than keeping the value.
    pub async fn resolve(&self, name: &str) -> Result<String> {
        if !self.allowed.iter().any(|a| a == name) {
            return Err(Error::Secret(name.to_string()));
        }
        let store = self
            .store
            .as_ref()
            .and_then(|s| s.as_secret_store())
            .ok_or_else(|| Error::Secret(name.into()))?;
        store
            .get(&self.tenant, name)
            .await?
            .ok_or_else(|| Error::Secret(name.to_string()))
    }
}

/// A request to create an agent, given to the provisioner.
#[derive(Clone, Debug)]
pub struct CreateRequest {
    /// Which configuration to start from, such as an agent file's name.
    pub template: String,
    /// The new agent's id; the provisioner may choose one when absent.
    pub agent_id: Option<AgentId>,
    /// Caller-supplied values the template may use.
    pub params: Value,
}

/// Everything needed to build a new agent.
#[derive(Clone, Debug)]
pub struct AgentSpec {
    pub config: AgentConfig,
    /// Slot blobs that replace runners' initial state.
    pub initial_state: Vec<(PluginId, Value)>,
    /// Events logged right after the header, such as a first message.
    pub initial_events: Vec<Event>,
}

/// Runtime-level service: produces each new agent's configuration and initial state.
#[async_trait]
pub trait Provisioner: Send + Sync {
    async fn provision(&self, request: CreateRequest) -> Result<AgentSpec>;
}

/// One loaded plugin of one agent.
pub struct PluginEntry {
    pub id: PluginId,
    pub runner: String,
    pub state_version: u32,
    pub grants: PluginGrants,
    pub plugin: Arc<dyn Plugin>,
}

/// An agent's loaded plugins, in config order, with indexes by hook and service.
pub struct PluginTable {
    entries: Vec<PluginEntry>,
    subscribers: HashMap<HookName, Vec<usize>>,
    tools: HashMap<String, usize>,
    driver: usize,
    queue: usize,
}

impl PluginTable {
    /// Indexes loaded plugins. Fails on duplicate ids or tool names, subscriptions outside
    /// `grants.hooks`, or anything other than exactly one driver and one message queue.
    pub fn build(entries: Vec<PluginEntry>) -> Result<Self> {
        let mut subscribers: HashMap<HookName, Vec<(i32, usize)>> = HashMap::new();
        let mut tools = HashMap::new();
        let (mut drivers, mut queues) = (Vec::new(), Vec::new());
        for (index, entry) in entries.iter().enumerate() {
            if entries[..index].iter().any(|e| e.id == entry.id) {
                return Err(Error::config(format!(
                    "plugin id `{}` is used twice",
                    entry.id
                )));
            }
            for sub in entry.plugin.subscriptions() {
                if let Some(allowed) = &entry.grants.hooks
                    && !allowed.contains(&sub.hook)
                {
                    return Err(Error::plugin(
                        &entry.id,
                        format!("hook {:?} is not in grants.hooks", sub.hook),
                    ));
                }
                subscribers
                    .entry(sub.hook)
                    .or_default()
                    .push((sub.order, index));
            }
            if let Some(tool) = entry.plugin.as_tool() {
                let name = tool.spec().name.clone();
                if tools.insert(name.clone(), index).is_some() {
                    return Err(Error::config(format!(
                        "tool name `{name}` is provided twice"
                    )));
                }
            }
            if entry.plugin.as_driver().is_some() {
                drivers.push(index);
            }
            if entry.plugin.as_queue().is_some() {
                queues.push(index);
            }
        }
        let exactly_one = |found: Vec<usize>, what: &str| match found.as_slice() {
            [one] => Ok(*one),
            _ => Err(Error::config(format!(
                "an agent needs exactly one {what} plugin, found {}",
                found.len()
            ))),
        };
        let driver = exactly_one(drivers, "driver")?;
        let queue = exactly_one(queues, "message queue")?;
        let subscribers = subscribers
            .into_iter()
            .map(|(hook, mut subs)| {
                subs.sort();
                (hook, subs.into_iter().map(|(_, i)| i).collect())
            })
            .collect();
        Ok(Self {
            entries,
            subscribers,
            tools,
            driver,
            queue,
        })
    }

    /// Every plugin in config order.
    pub fn entries(&self) -> &[PluginEntry] {
        &self.entries
    }

    /// Indexes of the plugins listening to `hook`, in dispatch order.
    pub fn subscribers(&self, hook: HookName) -> &[usize] {
        self.subscribers
            .get(&hook)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Position of a plugin in config order.
    pub fn index_of(&self, id: &PluginId) -> Option<usize> {
        self.entries.iter().position(|e| &e.id == id)
    }

    /// The plugin providing the named tool.
    pub fn tool(&self, name: &str) -> Option<&PluginEntry> {
        self.tools.get(name).map(|&i| &self.entries[i])
    }

    /// Specs of every tool, in config order.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.entries
            .iter()
            .filter_map(|e| e.plugin.as_tool())
            .map(|t| t.spec().clone())
            .collect()
    }

    /// The driver plugin.
    pub fn driver(&self) -> &Arc<dyn Plugin> {
        &self.entries[self.driver].plugin
    }

    /// The message queue plugin.
    pub fn queue(&self) -> &Arc<dyn Plugin> {
        &self.entries[self.queue].plugin
    }

    /// The model provider with this plugin id.
    pub fn model(&self, id: &PluginId) -> Result<Arc<dyn Plugin>> {
        let entry = self.index_of(id).map(|i| &self.entries[i]);
        match entry {
            Some(e) if e.plugin.as_model().is_some() => Ok(e.plugin.clone()),
            _ => Err(Error::config(format!(
                "plugin `{id}` is not a model provider"
            ))),
        }
    }
}
