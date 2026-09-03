//! ContextVM Gateway — bridge a local MCP server to Nostr.
//!
//! The gateway receives MCP requests via Nostr and forwards them to a local
//! MCP server, then publishes responses back to Nostr.

use crate::core::error::{Error, Result};
use crate::core::types::JsonRpcMessage;
use crate::payments::{with_server_payments, ServerPaymentsOptions};
use crate::transport::server::{IncomingRequest, NostrServerTransport, NostrServerTransportConfig};

/// Configuration for the gateway.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GatewayConfig {
    /// Nostr server transport configuration.
    pub nostr_config: NostrServerTransportConfig,
    /// Optional CEP-8 payments configuration. When set, the gateway registers
    /// payments on its transport via [`with_server_payments`] between
    /// constructing the transport and starting it.
    pub payment_options: Option<ServerPaymentsOptions>,
}

impl GatewayConfig {
    /// Create a new gateway configuration (payments stay opt-in via
    /// [`with_payment_options`](Self::with_payment_options)).
    pub fn new(nostr_config: NostrServerTransportConfig) -> Self {
        Self {
            nostr_config,
            payment_options: None,
        }
    }

    /// Set the CEP-8 payments configuration.
    pub fn with_payment_options(mut self, payment_options: ServerPaymentsOptions) -> Self {
        self.payment_options = Some(payment_options);
        self
    }
}

/// Gateway that bridges a local MCP server to Nostr.
///
/// The gateway listens for incoming MCP requests via Nostr, forwards them
/// to a local MCP handler function, and sends responses back over Nostr.
pub struct NostrMCPGateway {
    transport: NostrServerTransport,
    is_running: bool,
}

impl NostrMCPGateway {
    /// Create a new gateway.
    pub async fn new<T>(signer: T, config: GatewayConfig) -> Result<Self>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
    {
        let mut transport = NostrServerTransport::new(signer, config.nostr_config).await?;
        if let Some(payment_options) = config.payment_options {
            with_server_payments(&mut transport, payment_options)?;
        }

        Ok(Self {
            transport,
            is_running: false,
        })
    }

    /// Start the gateway. Returns a receiver for incoming requests.
    ///
    /// The caller is responsible for processing requests and calling
    /// `send_response` for each one.
    pub async fn start(&mut self) -> Result<tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>> {
        if self.is_running {
            return Err(Error::Other("Gateway already running".to_string()));
        }

        self.transport.start().await?;
        self.transport.spawn_discoverability_publication();
        self.is_running = true;

        self.transport
            .take_message_receiver()
            .ok_or_else(|| Error::Other("Message receiver already taken".to_string()))
    }

    /// Send a response back to the client for a given request.
    pub async fn send_response(&self, event_id: &str, response: JsonRpcMessage) -> Result<()> {
        self.transport.send_response(event_id, response).await
    }

    /// Publish server announcement.
    pub async fn announce(&self) -> Result<nostr_sdk::EventId> {
        self.transport.announce().await
    }

    /// Stop the gateway.
    ///
    /// The underlying transport is not restartable after close, so a stopped
    /// gateway cannot be started again (a later `start` panics at the transport's
    /// message-channel expect); payments registration neither causes nor changes
    /// this.
    pub async fn stop(&mut self) -> Result<()> {
        if !self.is_running {
            return Ok(());
        }
        self.transport.close().await?;
        self.is_running = false;
        Ok(())
    }

    /// Check if the gateway is active.
    pub fn is_active(&self) -> bool {
        self.is_running
    }
}

#[cfg(feature = "rmcp")]
impl NostrMCPGateway {
    /// Start a gateway directly from an rmcp server handler.
    ///
    /// This additive API keeps the existing `new/start/send_response` flow intact,
    /// while also allowing direct `handler.serve(transport)` style usage.
    pub async fn serve_handler<T, H>(
        signer: T,
        config: GatewayConfig,
        handler: H,
    ) -> Result<rmcp::service::RunningService<rmcp::RoleServer, H>>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
        H: rmcp::ServerHandler,
    {
        use crate::NostrServerTransport;
        use rmcp::ServiceExt;

        let mut transport = NostrServerTransport::new(signer, config.nostr_config).await?;
        if let Some(payment_options) = config.payment_options {
            with_server_payments(&mut transport, payment_options)?;
        }
        handler
            .serve(transport)
            .await
            .map_err(|e| Error::Other(format!("rmcp server initialization failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::*;
    use crate::transport::server::NostrServerTransportConfig;
    use std::time::Duration;

    #[test]
    fn test_gateway_config_construction() {
        let nostr_config = NostrServerTransportConfig {
            relay_urls: vec!["wss://relay.example.com".to_string()],
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: GiftWrapMode::Optional,
            server_info: Some(ServerInfo {
                name: Some("Test Gateway".to_string()),
                version: Some("1.0.0".to_string()),
                ..Default::default()
            }),
            is_announced_server: true,
            allowed_public_keys: vec!["abc123".to_string()],
            excluded_capabilities: vec![],
            max_sessions: 1000,
            cleanup_interval: Duration::from_secs(120),
            session_timeout: Duration::from_secs(600),
            request_timeout: Duration::from_secs(60),
            relay_list_urls: None,
            bootstrap_relay_urls: None,
            publish_relay_list: true,
            profile_metadata: None,
            oversized_transfer: Default::default(),
            open_stream: Default::default(),
        };

        let config = GatewayConfig {
            nostr_config,
            payment_options: None,
        };

        assert_eq!(
            config.nostr_config.relay_urls,
            vec!["wss://relay.example.com"]
        );
        assert!(config.payment_options.is_none());
        assert_eq!(
            config.nostr_config.encryption_mode,
            EncryptionMode::Required
        );
        assert!(config.nostr_config.is_announced_server);
        assert_eq!(config.nostr_config.allowed_public_keys.len(), 1);
        assert!(
            config
                .nostr_config
                .server_info
                .as_ref()
                .unwrap()
                .name
                .as_ref()
                .unwrap()
                == "Test Gateway"
        );
    }

    #[test]
    fn test_gateway_config_with_defaults() {
        let config = GatewayConfig {
            nostr_config: NostrServerTransportConfig::default(),
            payment_options: None,
        };
        assert_eq!(
            config.nostr_config.encryption_mode,
            EncryptionMode::Optional
        );
        assert!(!config.nostr_config.is_announced_server);
    }

    #[test]
    fn test_gateway_config_builder_sets_payment_options() {
        let config =
            GatewayConfig::new(NostrServerTransportConfig::default()).with_payment_options(
                crate::payments::ServerPaymentsOptions::new(Vec::new(), Vec::new()),
            );
        assert!(config.payment_options.is_some());
    }
}
