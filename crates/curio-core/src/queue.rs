//! The message queue service. Every message for the model waits in the agent's queue until
//! the agent pulls it.

use crate::error::Result;
use crate::event::{Delivery, QueuedMessage};
use crate::ids::{AgentId, QueueMsgId};
use async_trait::async_trait;

/// Provided by one plugin per agent. Its backend (a broker, a database, files) is the
/// plugin's code.
#[async_trait]
pub trait MessageQueue: Send + Sync {
    /// Becomes the only consumer of the agent's queue. Unacknowledged messages are
    /// delivered again to the new consumer.
    async fn attach(&self, agent: &AgentId) -> Result<Box<dyn QueueConsumer>>;

    /// Adds a message to the agent's queue, whether or not the agent is running.
    async fn publish(
        &self,
        agent: &AgentId,
        delivery: Delivery,
        body: String,
    ) -> Result<QueueMsgId>;
}

/// The attached consumer of one agent's queue.
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
