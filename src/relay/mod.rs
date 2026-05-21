//! Nostr relay pool management.
//!
//! Wraps nostr-sdk's Client for relay connection, event publishing, and subscription.

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;
#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockRelayPool;

use async_trait::async_trait;

use crate::core::error::{Error, Result};
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;

/// Trait abstracting relay pool operations, enabling dependency injection and testing.
#[async_trait]
pub trait RelayPoolTrait: Send + Sync {
    /// Connect to the given relay URLs.
    async fn connect(&self, relay_urls: &[String]) -> Result<()>;
    /// Disconnect from all relays.
    async fn disconnect(&self) -> Result<()>;
    /// Publish a pre-built event to relays.
    async fn publish_event(&self, event: &Event) -> Result<EventId>;
    /// Build, sign, and publish an event from a builder.
    async fn publish(&self, builder: EventBuilder) -> Result<EventId>;
    /// Sign an event builder without publishing.
    async fn sign(&self, builder: EventBuilder) -> Result<Event>;
    /// Get the signer associated with this relay pool.
    async fn signer(&self) -> Result<Arc<dyn NostrSigner>>;
    /// Get notifications receiver for event streaming.
    fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification>;
    /// Get the public key of the signer.
    async fn public_key(&self) -> Result<PublicKey>;
    /// Subscribe to events matching filters.
    async fn subscribe(&self, filters: Vec<Filter>) -> Result<()>;
    /// Sign and publish an event to specific relay URLs.
    async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId>;
    /// Fetch events matching a filter from connected relays.
    async fn fetch_events(&self, filter: Filter, timeout: Duration) -> Result<Vec<Event>>;
}

/// Relay pool wrapper for managing Nostr relay connections.
pub struct RelayPool {
    client: Arc<Client>,
}

impl RelayPool {
    /// Create a new relay pool with the given signer.
    pub async fn new<T>(signer: T) -> Result<Self>
    where
        T: IntoNostrSigner,
    {
        let client = Client::builder().signer(signer).build();

        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Connect to the given relay URLs.
    pub async fn connect(&self, relay_urls: &[String]) -> Result<()> {
        for url in relay_urls {
            self.client
                .add_relay(url)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
        }

        self.client.connect().await;

        Ok(())
    }

    /// Disconnect from all relays.
    pub async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await;
        Ok(())
    }

    /// Publish a pre-built event to relays.
    pub async fn publish_event(&self, event: &Event) -> Result<EventId> {
        let output = self
            .client
            .send_event(event)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(output.val)
    }

    /// Build, sign, and publish an event from a builder.
    pub async fn publish(&self, builder: EventBuilder) -> Result<EventId> {
        let output = self
            .client
            .send_event_builder(builder)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(output.val)
    }

    /// Sign an event builder without publishing.
    pub async fn sign(&self, builder: EventBuilder) -> Result<Event> {
        self.client
            .sign_event_builder(builder)
            .await
            .map_err(|e| Error::Transport(e.to_string()))
    }

    /// Get the underlying nostr-sdk Client.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// Get notifications receiver for event streaming.
    pub fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        self.client.notifications()
    }

    /// Get the public key of the signer.
    pub async fn public_key(&self) -> Result<PublicKey> {
        let signer = self
            .client
            .signer()
            .await
            .map_err(|e| Error::Other(e.to_string()))?;
        signer
            .get_public_key()
            .await
            .map_err(|e| Error::Other(e.to_string()))
    }

    /// Subscribe to events matching filters.
    pub async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
        for filter in filters {
            self.client
                .subscribe(filter, None)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
        }
        Ok(())
    }

    /// Sign and publish an event to specific relay URLs.
    pub async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId> {
        let output = self
            .client
            .send_event_builder_to(urls, builder)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(output.val)
    }

    /// Fetch events matching a filter from connected relays.
    pub async fn fetch_events(&self, filter: Filter, timeout: Duration) -> Result<Vec<Event>> {
        let events = self
            .client
            .fetch_events(filter, timeout)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(events.into_iter().collect())
    }
}

#[async_trait]
impl RelayPoolTrait for RelayPool {
    async fn connect(&self, relay_urls: &[String]) -> Result<()> {
        RelayPool::connect(self, relay_urls).await
    }

    async fn disconnect(&self) -> Result<()> {
        RelayPool::disconnect(self).await
    }

    async fn publish_event(&self, event: &Event) -> Result<EventId> {
        RelayPool::publish_event(self, event).await
    }

    async fn publish(&self, builder: EventBuilder) -> Result<EventId> {
        RelayPool::publish(self, builder).await
    }

    async fn sign(&self, builder: EventBuilder) -> Result<Event> {
        RelayPool::sign(self, builder).await
    }

    async fn signer(&self) -> Result<Arc<dyn NostrSigner>> {
        self.client
            .signer()
            .await
            .map_err(|e| Error::Other(e.to_string()))
    }

    fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
        RelayPool::notifications(self)
    }

    async fn public_key(&self) -> Result<PublicKey> {
        RelayPool::public_key(self).await
    }

    async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
        RelayPool::subscribe(self, filters).await
    }

    async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId> {
        RelayPool::publish_to(self, urls, builder).await
    }

    async fn fetch_events(&self, filter: Filter, timeout: Duration) -> Result<Vec<Event>> {
        RelayPool::fetch_events(self, filter, timeout).await
    }
}
