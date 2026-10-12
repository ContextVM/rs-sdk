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

/// Log target for relay pool internals.
const LOG_TARGET: &str = "contextvm_sdk::relay";

/// How long `publish_to` waits for at least one socket before sending.
const PUBLISH_TO_CONNECT_WAIT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for the first socket.
const PUBLISH_TO_CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long a send round waits for relay acknowledgements before giving up.
const PUBLISH_TO_SEND_WAIT: Duration = Duration::from_secs(15);

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
    /// Fetch events matching filters from connected relays.
    async fn fetch_events(&self, filters: Vec<Filter>, timeout: Duration) -> Result<Vec<Event>>;
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
    ///
    /// Publishes through a short-lived dedicated connection to exactly these
    /// relays — they do not need to be members of this pool. Waits up to 10
    /// seconds for at least one socket before sending, so a still-connecting
    /// relay reports a real outcome instead of a silent no-answer, and up to
    /// 15 seconds for acknowledgements before giving up on the send. Per-relay
    /// refusals are logged with the relay's own reason;
    /// returns [`Error::Transport`] when no relay accepted the event.
    pub async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId> {
        let event = self.sign(builder).await?;
        let client = Client::builder().build();
        for url in urls {
            client
                .add_relay(url.as_str())
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
        }
        client.connect().await;

        // Bounded wait for the first connected socket before the send round.
        let deadline = tokio::time::Instant::now() + PUBLISH_TO_CONNECT_WAIT;
        loop {
            let relays = client.relays().await;
            if relays.is_empty()
                || relays
                    .values()
                    .any(|r| matches!(r.status(), RelayStatus::Connected))
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(PUBLISH_TO_CONNECT_POLL_INTERVAL).await;
        }

        // Bounded send: a relay that accepts the socket but never
        // acknowledges cannot stall past the caller's round backstop, so the
        // teardown below stays reachable on every path.
        let result = async {
            let output = match tokio::time::timeout(
                PUBLISH_TO_SEND_WAIT,
                client.send_event_to(urls, &event),
            )
            .await
            {
                Ok(output) => output.map_err(|e| Error::Transport(e.to_string()))?,
                Err(_) => {
                    return Err(Error::Transport(format!(
                        "no relay answered within {} seconds",
                        PUBLISH_TO_SEND_WAIT.as_secs()
                    )))
                }
            };
            for (url, reason) in &output.failed {
                tracing::warn!(
                    target: LOG_TARGET,
                    relay = %url,
                    reason = %reason,
                    "Relay refused discoverability event"
                );
            }
            if output.success.is_empty() {
                return Err(Error::Transport(format!(
                    "no relay accepted the event: {:?}",
                    output.failed
                )));
            }
            Ok(output.val)
        }
        .await;

        client.disconnect().await;
        result
    }

    /// Fetch events matching filters from connected relays.
    pub async fn fetch_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
    ) -> Result<Vec<Event>> {
        let mut all_events = Vec::new();
        for filter in filters {
            let events = self
                .client
                .fetch_events(filter, timeout)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
            all_events.extend(events);
        }
        Ok(all_events)
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

    async fn fetch_events(&self, filters: Vec<Filter>, timeout: Duration) -> Result<Vec<Event>> {
        RelayPool::fetch_events(self, filters, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// A relay that accepts TCP but never speaks WebSocket must not hang
    /// `publish_to`: the connect-wait self-bounds and the failure surfaces
    /// instead of being reported as success.
    #[tokio::test(start_paused = true)]
    async fn publish_to_self_bounds_when_relay_never_connects() {
        // Black-hole relay: accept connections and hold them, never respond.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let pool = RelayPool::new(Keys::generate()).await.unwrap();
        let url = format!("ws://{}", addr);

        // 27 s: publish_to's own budget is 10 s connect-wait + 15 s send.
        let outcome = tokio::time::timeout(
            Duration::from_secs(27),
            pool.publish_to(&[url], EventBuilder::new(Kind::Custom(0), "{}")),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "publish_to must self-bound before the 30 s round backstop cancels it"
        );
        assert!(
            outcome.unwrap().is_err(),
            "a relay that never answers must not report success"
        );
    }

    /// A relay that completes the WebSocket handshake but never acknowledges
    /// events must not hang `publish_to`: the send self-bounds and the failure
    /// surfaces instead of being reported as success. This pins the bound that
    /// keeps `publish_to`'s teardown reachable before the round backstop.
    #[tokio::test(start_paused = true)]
    async fn publish_to_self_bounds_against_a_silent_relay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                // Complete the handshake, then read forever and never reply.
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while ws.next().await.is_some() {}
                });
            }
        });

        let pool = RelayPool::new(Keys::generate()).await.unwrap();
        let url = format!("ws://{}", addr);

        let outcome = tokio::time::timeout(
            Duration::from_secs(27),
            pool.publish_to(&[url], EventBuilder::new(Kind::Custom(0), "{}")),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "publish_to must self-bound before the 30 s round backstop cancels it"
        );
        assert!(
            outcome.unwrap().is_err(),
            "a relay that never acknowledges must not report success"
        );
    }
}
