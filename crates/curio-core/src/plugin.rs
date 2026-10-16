//! Runners, plugins, state slots and the service interfaces plugins provide.
//!
//! A plugin is a pair: a [`Runner`], which is Rust code compiled into the host and shared
//! by every agent, and code, which is whatever that runner interprets (for the built-in
//! runners, a configuration table). [`Runner::load`] turns code into a [`LoadedPlugin`]: an
//! immutable [`Plugin`] instance for one agent plus its initial [`PluginState`]. The
//! instance never changes after load; everything it needs to remember goes in its state
//! slot, which lives in the agent's context and is serialized into every image. That split
//! is what lets the harness checkpoint any plugin without the plugin's help.
//!
//! A plugin takes part in two ways. It can listen to hooks ([`Plugin::subscriptions`] and
//! [`Plugin::on_hook`]), and it can provide services, which are called instead of notified:
//! the [`Driver`] that runs the loop, a [`ModelProvider`], a [`Tool`], the agent's
//! [`crate::MessageQueue`], and the three runtime-level services ([`crate::LogStore`],
//! [`Provisioner`], [`SecretStore`]). A plugin offers a service by overriding the matching
//! `as_*` method to return itself; the core finds services only through those methods.
//!
//! [`crate::Runtime`] loads an agent's plugins in config order into [`PluginEntry`]s and
//! indexes them in a [`PluginTable`], which checks the agent's shape once (unique ids and
//! tool names, granted hooks, exactly one driver and one queue) so the loop never has to.
//! Plugins reach the outside world only through what the runtime hands them: the
//! [`LoadEnv`] at load time, with the plugin's grants and a [`SecretScope`], and the
//! [`ToolCx`] given to each tool call.

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
///
/// Registered in a [`crate::RunnerRegistry`] and called by the runtime whenever an agent is
/// created, thawed or recovered, and once at start for each runtime-level plugin.
#[async_trait]
pub trait Runner: Send + Sync + 'static {
    /// The runner name agent configs refer to, such as `builtin.tool.shell`.
    fn name(&self) -> &str;

    /// Version of the state slot format this runner writes. Bumped on incompatible change.
    /// Recorded with every slot in images and in the log header.
    fn state_version(&self) -> u32 {
        1
    }

    /// Validates and loads `code` for one plugin. Misconfiguration fails here, at create,
    /// thaw or recovery, never later in the loop.
    ///
    /// Called again on every thaw and recovery with the same code, so loading must not have
    /// side effects that matter when repeated. The returned initial state is used for a new
    /// agent and for recovery; thaw replaces it with the image's slot.
    async fn load(&self, code: &Value, env: &LoadEnv) -> Result<LoadedPlugin>;

    /// Upgrades a state slot written by an older version of this runner. Called on thaw
    /// when an image's slot version is lower than [`Runner::state_version`]. The default
    /// refuses, which makes such a thaw fail with a configuration error.
    fn migrate(&self, _blob: Value, from: u32) -> Result<Value> {
        Err(Error::config(format!(
            "runner `{}` cannot migrate state from version {from} to {}",
            self.name(),
            self.state_version()
        )))
    }
}

/// What a runner gets besides the code: who the plugin is for and what it may reach.
///
/// Built by the runtime for each load. Runners copy what their plugin needs out of it; it
/// isn't kept by the core after `load` returns.
#[derive(Clone)]
pub struct LoadEnv {
    /// The plugin's id: from the agent config, or `log_store`, `provisioner` or
    /// `secret_store` for runtime-level plugins.
    pub plugin: PluginId,
    /// `None` for runtime-level plugins.
    pub agent: Option<AgentId>,
    /// The agent's tenant, or `host` for runtime-level plugins.
    pub tenant: TenantId,
    /// What the plugin may touch, from its spec.
    pub grants: PluginGrants,
    /// Relative paths in code resolve against this directory (the host config's directory).
    pub base_dir: PathBuf,
    /// The secrets the plugin may resolve.
    pub secrets: SecretScope,
}

impl LoadEnv {
    /// Resolves a path from code against the base directory. An absolute path stays as it
    /// is. This doesn't check `grants.fs`.
    pub fn resolve_path(&self, path: impl AsRef<std::path::Path>) -> PathBuf {
        self.base_dir.join(path)
    }

    /// Fails unless `host` is in `grants.hosts`. Runners that connect somewhere call it at
    /// load time, so an ungranted host is a load error. The match is exact.
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

    /// Parses the code as the runner's configuration type. Absent code (`null`) parses as
    /// an empty table, so a config type whose fields all have defaults needs no code.
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
    /// The plugin instance, immutable from here on.
    pub instance: Arc<dyn Plugin>,
    /// The state slot a new agent starts with.
    pub initial_state: Box<dyn PluginState>,
}

impl LoadedPlugin {
    /// A plugin whose state slot is empty (`()`), which matches the default
    /// [`Plugin::decode_state`].
    pub fn stateless(instance: Arc<dyn Plugin>) -> Self {
        Self {
            instance,
            initial_state: Box::new(()),
        }
    }
}

/// The mutable, serializable state of one plugin instance. Stored in the agent's context,
/// carried in images.
///
/// Implemented for every `Serialize + Send + Sync + 'static` type, so a runner just picks a
/// serde type. Its listeners reach it through [`HookCx::state`]; nothing else can.
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

/// Decodes a state slot blob as `T`; for [`Plugin::decode_state`] implementations. Fails
/// with [`Error::Json`] when the blob doesn't match `T`.
pub fn decode_state<T: DeserializeOwned + Serialize + Any + Send + Sync>(
    blob: Value,
) -> Result<Box<dyn PluginState>> {
    Ok(Box::new(serde_json::from_value::<T>(blob)?))
}

/// One plugin for one agent. Immutable after load: hooks take `&self`, and mutable state
/// lives in the slot [`HookCx::state`] exposes.
///
/// Every method has a default, so a plugin implements only what it uses: listeners override
/// `subscriptions` and `on_hook`, service providers override the matching `as_*` method,
/// and stateful plugins override `decode_state`.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// Hooks this plugin listens to. Read once, when the plugin table is built; each must be
    /// allowed by the plugin's `grants.hooks`.
    fn subscriptions(&self) -> Vec<Subscription> {
        Vec::new()
    }

    /// Handles one hook dispatch. Called only for subscribed hooks. An error fails the
    /// dispatch and, through the driver, stops the agent; return [`Flow::Continue`] for
    /// anything the plugin doesn't need to act on.
    async fn on_hook(&self, _cx: &mut HookCx<'_>, _event: &mut HookEvent<'_>) -> Result<Flow> {
        Ok(Flow::Continue)
    }

    /// Decodes a slot blob from an image, or from a provisioner's initial state. Must accept
    /// what this plugin's state type writes in [`PluginState::to_blob`]. The default expects
    /// an empty slot.
    fn decode_state(&self, blob: Value) -> Result<Box<dyn PluginState>> {
        decode_state::<()>(blob)
    }

    /// The plugin as the agent's loop, if it is one. An agent needs exactly one.
    fn as_driver(&self) -> Option<&dyn Driver> {
        None
    }

    /// The plugin as a model provider, if it is one. Routes name it by plugin id.
    fn as_model(&self) -> Option<&dyn ModelProvider> {
        None
    }

    /// The plugin as a tool, if it is one. One plugin provides at most one tool.
    fn as_tool(&self) -> Option<&dyn Tool> {
        None
    }

    /// The plugin as the agent's message queue, if it is one. An agent needs exactly one.
    fn as_queue(&self) -> Option<&dyn MessageQueue> {
        None
    }

    /// The plugin as the runtime's log store, if it is one.
    fn as_log_store(&self) -> Option<&dyn LogStore> {
        None
    }

    /// The plugin as the runtime's provisioner, if it is one.
    fn as_provisioner(&self) -> Option<&dyn Provisioner> {
        None
    }

    /// The plugin as the runtime's secret store, if it is one.
    fn as_secret_store(&self) -> Option<&dyn SecretStore> {
        None
    }
}

/// The loop. Replacing the driver plugin replaces the loop.
///
/// The runtime's agent task calls `run` once with the agent's context and ends when it
/// returns. A driver is expected to dispatch `agent_loaded` first, continue a turn left
/// open by a step-boundary drain, follow the save points the log requires, apply commands
/// and settle drains at its safe points, and keep the view changing only through the
/// journal. `curio-builtins` has the default one.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Runs the agent until it quiesces or closes. An error stops the agent; the runtime
    /// reports it to observers and drain waiters, and the agent can be recovered from its
    /// log.
    async fn run(&self, cx: &mut AgentCtx) -> Result<Exit>;
}

/// A model provider. Streams partial output to `observer`; returns the complete reply.
///
/// Called once per attempt by the driver, inside [`crate::AgentCtx::wait`], so commands are
/// handled while it runs. It owns no retry logic; retries are the `request_error` hook's
/// job. Secrets such as API keys come from the plugin's [`SecretScope`], resolved per
/// request.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Runs one attempt. Returns [`crate::ModelErrorKind::Cancelled`] soon after `cancel` fires.
    /// Whatever it streams to `observer` is display only; the returned reply is what's
    /// logged.
    async fn complete(
        &self,
        request: ModelRequest,
        observer: Observer,
        cancel: CancellationToken,
    ) -> std::result::Result<AssistantMessage, ModelError>;
}

/// What a tool body gets: read-only facts about the call and the plugin's grants.
///
/// Made by [`crate::AgentCtx::tool_cx`] for each call. It owns its data instead of
/// borrowing the context, so tool bodies can run concurrently while the driver holds the
/// context. A tool can't change the agent from here; its effect on the conversation is the
/// output it returns.
#[derive(Clone)]
pub struct ToolCx {
    /// The calling agent.
    pub agent: AgentId,
    /// The agent's tenant.
    pub tenant: TenantId,
    /// The call's id; use it as an idempotency key where the target supports one.
    pub call_id: CallId,
    /// The tool plugin's grants.
    pub grants: PluginGrants,
    /// Cancelled when the turn is cancelled or a drain is forced.
    pub cancel: CancellationToken,
}

/// A tool the model can call.
#[async_trait]
pub trait Tool: Send + Sync {
    /// What the model is told about the tool, and how calls may be scheduled. Must not
    /// change after load.
    fn spec(&self) -> &ToolSpec;

    /// Runs the tool body. Must return soon after `cx.cancel` fires.
    ///
    /// `args` is the call's arguments parsed as JSON (an empty object when the model sent
    /// none); calls with invalid JSON never get here.
    /// Failures are returned as a [`ToolOutput::error`] for the model to read. The driver
    /// logs `ToolStarted` and syncs before calling this, and may also stop it at a timeout,
    /// so a tool with outside effects should make them idempotent by `cx.call_id`.
    async fn call(&self, cx: ToolCx, args: Value) -> ToolOutput;
}

/// Runtime-level service: resolves a tenant's secret names to values.
///
/// Plugins never call it directly; they go through their [`SecretScope`], which checks
/// grants first.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// The value of `name` for `tenant`, or `None` if it isn't defined.
    async fn get(&self, tenant: &TenantId, name: &str) -> Result<Option<String>>;
}

/// The secrets one plugin may resolve: its tenant's, limited to `grants.secrets`.
///
/// Handed to the runner in [`LoadEnv::secrets`]. Plugins keep the scope and resolve each
/// secret when they need it, so values stay out of plugin instances, state slots, logs and
/// images.
#[derive(Clone)]
pub struct SecretScope {
    /// The secret store plugin; `None` for the secret store's own scope.
    store: Option<Arc<dyn Plugin>>,
    /// The tenant whose secrets this scope resolves.
    tenant: TenantId,
    /// Secret names the plugin is granted.
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
    ///
    /// Fails with [`Error::Secret`] when the name isn't granted, the scope has no store, or
    /// the store doesn't define it, so a plugin can't tell those cases apart.
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

/// A request to create an agent, given to the provisioner by [`crate::Runtime::create`].
#[derive(Clone, Debug)]
pub struct CreateRequest {
    /// Which configuration to start from, such as an agent file's name.
    pub template: String,
    /// The new agent's id; the provisioner may choose one when absent.
    pub agent_id: Option<AgentId>,
    /// Caller-supplied values the template may use.
    pub params: Value,
}

/// Everything needed to build a new agent. What a [`Provisioner`] returns.
#[derive(Clone, Debug)]
pub struct AgentSpec {
    /// The new agent's configuration, which gets pinned in its log header.
    pub config: AgentConfig,
    /// Slot blobs that replace runners' initial state, decoded with each plugin's
    /// [`Plugin::decode_state`]. Every id must name a plugin in `config`.
    pub initial_state: Vec<(PluginId, Value)>,
    /// Events logged right after the header, such as a first message. They're folded like
    /// any other event, so they shape the view the agent starts with.
    pub initial_events: Vec<Event>,
}

/// Runtime-level service: produces each new agent's configuration and initial state.
///
/// Asked once per agent, at creation. Thaw and recovery never call it: the agent's config
/// comes from its image or log header, so changing the provisioner doesn't change existing
/// agents.
#[async_trait]
pub trait Provisioner: Send + Sync {
    /// Builds the spec for a new agent. Fails if the template is unknown or invalid.
    async fn provision(&self, request: CreateRequest) -> Result<AgentSpec>;
}

/// One loaded plugin of one agent.
pub struct PluginEntry {
    /// The plugin's id from the agent config.
    pub id: PluginId,
    /// The runner that loaded it.
    pub runner: String,
    /// The runner's state version at load; what the plugin's slot is written as.
    pub state_version: u32,
    /// What the plugin may touch; copied into each [`ToolCx`] for a tool.
    pub grants: PluginGrants,
    /// The instance.
    pub plugin: Arc<dyn Plugin>,
}

/// An agent's loaded plugins, in config order, with indexes by hook and service.
///
/// Immutable once built, and shared through an `Arc` in [`crate::AgentCtx::plugins`]. An
/// entry's position is also the index of its state slot in [`crate::AgentCtx::state`].
pub struct PluginTable {
    /// Every plugin in config order.
    entries: Vec<PluginEntry>,
    /// For each hook, the indexes of its listeners in dispatch order.
    subscribers: HashMap<HookName, Vec<usize>>,
    /// Tool name to the index of the plugin providing it.
    tools: HashMap<String, usize>,
    /// Index of the driver plugin.
    driver: usize,
    /// Index of the message queue plugin.
    queue: usize,
}

impl PluginTable {
    /// Indexes loaded plugins. Fails on duplicate ids or tool names, subscriptions outside
    /// `grants.hooks`, or anything other than exactly one driver and one message queue.
    ///
    /// Each hook's listeners are sorted by `(order, position)`, so equal orders keep config
    /// order.
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

    /// The model provider with this plugin id. Fails with a configuration error if there's
    /// no such plugin or it provides no model; the driver checks its default route at start.
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
