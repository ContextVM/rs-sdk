//! ContextVM Proxy — connect to a remote Nostr MCP server as if local.
//!
//! The proxy sends MCP requests over Nostr to a remote server and
//! receives responses, making the remote server accessible locally.

use crate::core::error::{Error, Result};
use crate::core::types::JsonRpcMessage;
use crate::payments::client_payments::{with_client_payments, ClientPaymentsOptions};
use crate::transport::client::{NostrClientTransport, NostrClientTransportConfig};

/// Configuration for the proxy.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProxyConfig {
    /// Nostr client transport configuration.
    pub nostr_config: NostrClientTransportConfig,
    /// CEP-8: client payment configuration. When `Some`, the proxy registers
    /// payments on its transport via
    /// [`with_client_payments`] at construction, before the transport
    /// starts. `None` (the default) leaves
    /// the transport's own payment configuration untouched.
    pub payment_options: Option<ClientPaymentsOptions>,
}

impl ProxyConfig {
    /// Create a new proxy configuration.
    pub fn new(nostr_config: NostrClientTransportConfig) -> Self {
        Self {
            nostr_config,
            payment_options: None,
        }
    }

    /// CEP-8: register client payments on the proxy's transport at
    /// construction.
    pub fn with_payment_options(mut self, options: ClientPaymentsOptions) -> Self {
        self.payment_options = Some(options);
        self
    }
}

/// Proxy that connects to a remote MCP server via Nostr.
pub struct NostrMCPProxy {
    transport: NostrClientTransport,
    is_running: bool,
}

impl NostrMCPProxy {
    /// Create a new proxy.
    pub async fn new<T>(signer: T, config: ProxyConfig) -> Result<Self>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
    {
        let mut transport = NostrClientTransport::new(signer, config.nostr_config).await?;
        // CEP-8: register payments between construction and ownership, while
        // the transport is still pre-start. Registering in `start()` instead
        // would carry a real trap: a first `start()` that consumed the options
        // and then failed at the transport would leave a second `start()`
        // silently skipping registration, shipping a non-paying client.
        if let Some(payment_options) = config.payment_options {
            with_client_payments(&mut transport, payment_options)?;
        }

        Ok(Self {
            transport,
            is_running: false,
        })
    }

    /// Start the proxy. Returns a receiver for incoming responses/notifications.
    pub async fn start(&mut self) -> Result<tokio::sync::mpsc::UnboundedReceiver<JsonRpcMessage>> {
        if self.is_running {
            return Err(Error::Other("Proxy already running".to_string()));
        }

        self.transport.start().await?;
        self.is_running = true;

        self.transport
            .take_message_receiver()
            .ok_or_else(|| Error::Other("Message receiver already taken".to_string()))
    }

    /// Send an MCP request to the remote server.
    pub async fn send(&self, message: &JsonRpcMessage) -> Result<()> {
        self.transport.send(message).await
    }

    /// Stop the proxy.
    pub async fn stop(&mut self) -> Result<()> {
        if !self.is_running {
            return Ok(());
        }
        self.transport.close().await?;
        self.is_running = false;
        Ok(())
    }

    /// Check if the proxy is active.
    pub fn is_active(&self) -> bool {
        self.is_running
    }
}

#[cfg(feature = "rmcp")]
impl NostrMCPProxy {
    /// Start a proxy directly from an rmcp client handler.
    ///
    /// This additive API keeps the existing `new/start/send` flow intact,
    /// while also allowing direct `handler.serve(transport)` style usage.
    pub async fn serve_client_handler<T, H>(
        signer: T,
        config: ProxyConfig,
        handler: H,
    ) -> Result<rmcp::service::RunningService<rmcp::RoleClient, H>>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
        H: rmcp::ClientHandler,
    {
        use crate::NostrClientTransport;
        use rmcp::ServiceExt;

        let mut transport = NostrClientTransport::new(signer, config.nostr_config).await?;
        // CEP-8: the rmcp-direct path constructs its own transport, so it
        // registers payments here too; without this, this path would silently
        // ship a non-paying client. Registration is genuinely pre-start: the
        // rmcp worker starts the transport only after serving begins.
        if let Some(payment_options) = config.payment_options {
            with_client_payments(&mut transport, payment_options)?;
        }
        handler
            .serve(transport)
            .await
            .map_err(|e| Error::Other(format!("rmcp client initialization failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::*;
    use crate::transport::client::NostrClientTransportConfig;
    use std::time::Duration;

    #[test]
    fn test_proxy_config_construction() {
        let keys = nostr_sdk::Keys::generate();
        let server_pubkey = keys.public_key().to_hex();

        let nostr_config = NostrClientTransportConfig {
            relay_urls: vec!["wss://relay.example.com".to_string()],
            server_pubkey: server_pubkey.clone(),
            encryption_mode: EncryptionMode::Required,
            gift_wrap_mode: GiftWrapMode::Optional,
            is_stateless: true,
            timeout: Duration::from_secs(60),
            discovery_relay_urls: None,
            fallback_operational_relay_urls: None,
            oversized_transfer: Default::default(),
            open_stream: Default::default(),
            payment_interaction: None,
            pmis: vec![],
        };

        let config = ProxyConfig {
            nostr_config,
            payment_options: None,
        };

        assert_eq!(
            config.nostr_config.relay_urls,
            vec!["wss://relay.example.com"]
        );
        assert_eq!(config.nostr_config.server_pubkey, server_pubkey);
        assert_eq!(
            config.nostr_config.encryption_mode,
            EncryptionMode::Required
        );
        assert!(config.nostr_config.is_stateless);
        assert_eq!(config.nostr_config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn payment_options_default_none_and_builder_sets_some() {
        let config = ProxyConfig::new(NostrClientTransportConfig::default());
        assert!(
            config.payment_options.is_none(),
            "no payment registration unless asked"
        );
        let config = ProxyConfig::new(NostrClientTransportConfig::default())
            .with_payment_options(crate::payments::ClientPaymentsOptions::new());
        assert!(config.payment_options.is_some());
    }

    #[test]
    fn test_proxy_config_with_defaults() {
        let config = ProxyConfig {
            nostr_config: NostrClientTransportConfig::default(),
            payment_options: None,
        };
        assert!(!config.nostr_config.is_stateless);
        assert_eq!(
            config.nostr_config.encryption_mode,
            EncryptionMode::Optional
        );
    }
}
