//! ContextVM Gateway — bridge a local MCP server to Nostr.
//!
//! The gateway receives MCP requests via Nostr and forwards them to a local
//! MCP server, then publishes responses back to Nostr.

use crate::core::error::{Error, Result};
use crate::core::types::JsonRpcMessage;
use crate::transport::server::{IncomingRequest, NostrServerTransport, NostrServerTransportConfig};

/// Configuration for the gateway.
pub struct GatewayConfig {
    /// Nostr server transport configuration.
    pub nostr_config: NostrServerTransportConfig,
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
        let transport = NostrServerTransport::new(signer, config.nostr_config).await?;

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
    /// # Deprecated
    ///
    /// This method creates a single worker that can only serve one client
    /// at a time. Use [`serve_handler_pooled`](Self::serve_handler_pooled)
    /// for production deployments that need to handle multiple concurrent clients.
    #[deprecated(
        since = "0.2.0",
        note = "Use `serve_handler_pooled` for multi-client support with capacity management"
    )]
    pub async fn serve_handler<T, H>(
        signer: T,
        config: GatewayConfig,
        handler: H,
    ) -> Result<rmcp::service::RunningService<rmcp::RoleServer, H>>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
        H: rmcp::ServerHandler,
    {
        #[allow(deprecated)]
        use crate::rmcp_transport::NostrServerWorker;
        use rmcp::ServiceExt;

        #[allow(deprecated)]
        let worker = NostrServerWorker::new(signer, config.nostr_config).await?;
        handler
            .serve(worker)
            .await
            .map_err(|e| Error::Other(format!("rmcp server initialization failed: {e}")))
    }

    /// Start a gateway with a worker pool that serves multiple clients concurrently.
    ///
    /// Each worker in the pool gets a fresh handler instance created by the
    /// `handler_factory` function. Incoming clients are distributed across
    /// workers using round-robin assignment with client affinity (a client's
    /// messages always go to the same worker once assigned).
    ///
    /// # Arguments
    ///
    /// * `signer` — The Nostr signer for the server identity
    /// * `config` — Gateway configuration (wraps `NostrServerTransportConfig`)
    /// * `pool_config` — Worker pool sizing and capacity settings
    /// * `handler_factory` — A function that creates a new handler instance per worker
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use contextvm_sdk::gateway::{NostrMCPGateway, GatewayConfig};
    /// use contextvm_sdk::rmcp_transport::WorkerPoolConfig;
    ///
    /// # async fn example() -> contextvm_sdk::Result<()> {
    /// // let handle = NostrMCPGateway::serve_handler_pooled(
    /// //     keys,
    /// //     config,
    /// //     WorkerPoolConfig::default(),
    /// //     || MyHandler::new(),
    /// // ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn serve_handler_pooled<T, H, F>(
        signer: T,
        config: GatewayConfig,
        pool_config: crate::rmcp_transport::WorkerPoolConfig,
        handler_factory: F,
    ) -> Result<crate::rmcp_transport::WorkerPoolHandle>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
        H: rmcp::ServerHandler,
        F: Fn() -> H,
    {
        crate::rmcp_transport::start_worker_pool(
            signer,
            config.nostr_config,
            pool_config,
            handler_factory,
        )
        .await
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
            server_info: Some(ServerInfo {
                name: Some("Test Gateway".to_string()),
                version: Some("1.0.0".to_string()),
                ..Default::default()
            }),
            is_announced_server: true,
            allowed_public_keys: vec!["abc123".to_string()],
            excluded_capabilities: vec![],
            cleanup_interval: Duration::from_secs(120),
            session_timeout: Duration::from_secs(600),
            log_file_path: None,
        };

        let config = GatewayConfig { nostr_config };

        assert_eq!(
            config.nostr_config.relay_urls,
            vec!["wss://relay.example.com"]
        );
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
        };
        assert_eq!(
            config.nostr_config.encryption_mode,
            EncryptionMode::Optional
        );
        assert!(!config.nostr_config.is_announced_server);
    }
}
