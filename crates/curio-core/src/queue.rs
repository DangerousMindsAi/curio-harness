//! The message queue service. Every message for the model waits in the agent's queue until
//! the agent pulls it.
//!
//! Messages from users, other agents or webhooks never travel to an agent as commands. A
//! sender publishes them with [`MessageQueue::publish`] (usually through
//! [`crate::AgentHandle::send`]), whether or not the agent is running anywhere. The agent's
//! task attaches as the queue's only [`QueueConsumer`] when it starts and pulls messages
//! only when it's ready to use them, in [`crate::AgentCtx::pull_messages`]: when idle, it
//! waits for one; between steps, it takes whatever has arrived without waiting.
//!
//! Delivery is at least once, and the log makes it effectively once. The agent logs each
//! pulled message as [`crate::Event::MessageReceived`], syncs the log, and only then
//! acknowledges, so a message leaves the queue only after it's durable in the log. If the
//! agent stops between the sync and the ack, the next consumer gets the message again and
//! the agent skips it because its id is no newer than the last one logged. That depends on
//! queue ids increasing within one agent's queue.
//!
//! A draining agent stops pulling, so anything sent while it's quiesced or moving between
//! hosts waits in the queue for whoever thaws it.

use crate::error::Result;
use crate::event::{Delivery, QueuedMessage};
use crate::ids::{AgentId, QueueMsgId};
use async_trait::async_trait;

/// Provided by one plugin per agent. Its backend (a broker, a database, files) is the
/// plugin's code.
///
/// The same plugin instance serves both sides: the agent's handle uses it to publish, and
/// the agent's task uses it to attach. It must be safe to call from several tasks at once.
#[async_trait]
pub trait MessageQueue: Send + Sync {
    /// Becomes the only consumer of the agent's queue. Unacknowledged messages are
    /// delivered again to the new consumer.
    async fn attach(&self, agent: &AgentId) -> Result<Box<dyn QueueConsumer>>;

    /// Adds a message to the agent's queue, whether or not the agent is running. Returns the
    /// id the queue assigned, which is greater than every earlier id in this agent's queue.
    async fn publish(
        &self,
        agent: &AgentId,
        delivery: Delivery,
        body: String,
    ) -> Result<QueueMsgId>;
}

/// The attached consumer of one agent's queue. Owned by the agent's context while the agent
/// runs, and detached when it quiesces.
#[async_trait]
pub trait QueueConsumer: Send + Sync {
    /// Returns up to `max` messages after the last one pulled, oldest first. With `wait`,
    /// waits until at least one exists. Cancel-safe: dropping the future loses nothing.
    async fn pull(&mut self, max: usize, wait: bool) -> Result<Vec<QueuedMessage>>;

    /// Removes every message up to and including `upto` from the queue.
    async fn ack(&mut self, upto: QueueMsgId) -> Result<()>;

    /// Stops consuming. Unacknowledged messages stay for the next consumer.
    async fn detach(self: Box<Self>) -> Result<()>;
}
