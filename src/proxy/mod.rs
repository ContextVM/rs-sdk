//! ContextVM Proxy — connect to a remote Nostr MCP server as if local.
//!
//! The proxy sends MCP requests over Nostr to a remote server and
//! receives responses, making the remote server accessible locally.

use crate::core::error::{Error, Result};
use crate::core::types::JsonRpcMessage;
use crate::transport::client::{NostrClientTransport, NostrClientTransportConfig};

/// Configuration for the proxy.
pub struct ProxyConfig {
    /// Nostr client transport configuration.
    pub nostr_config: NostrClientTransportConfig,
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
        let transport = NostrClientTransport::new(signer, config.nostr_config).await?;

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
    /// while allowing rmcp-first usage through the worker adapter.
    pub async fn serve_client_handler<T, H>(
        signer: T,
        config: ProxyConfig,
        handler: H,
    ) -> Result<rmcp::service::RunningService<rmcp::RoleClient, H>>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
        H: rmcp::ClientHandler,
    {
        use crate::rmcp_transport::NostrClientWorker;
        use rmcp::ServiceExt;

        let worker = NostrClientWorker::new(signer, config.nostr_config).await?;
        handler
            .serve(worker)
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
            is_stateless: true,
            timeout: Duration::from_secs(60),
            log_file_path: None,
        };

        let config = ProxyConfig { nostr_config };

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
    fn test_proxy_config_with_defaults() {
        let config = ProxyConfig {
            nostr_config: NostrClientTransportConfig::default(),
        };
        assert!(!config.nostr_config.is_stateless);
        assert_eq!(
            config.nostr_config.encryption_mode,
            EncryptionMode::Optional
        );
    }
}
