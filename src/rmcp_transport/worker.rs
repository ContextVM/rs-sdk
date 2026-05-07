//! rmcp worker adapters.
//!
//! This file defines wrapper types that bind existing ContextVM Nostr
//! transports to rmcp's worker abstraction.

use crate::core::error::Result;
use crate::core::types::JsonRpcMessage;
use crate::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use crate::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use rmcp::transport::worker::{Worker, WorkerContext, WorkerQuitReason};

use super::convert::{
    internal_to_rmcp_client_rx, internal_to_rmcp_server_rx, rmcp_client_tx_to_internal,
    rmcp_server_tx_to_internal,
};

const LOG_TARGET: &str = "contextvm_sdk::rmcp_transport::worker";

/// rmcp server worker wrapper for ContextVM Nostr server transport.
///
/// Multiplexes all connected clients through a single rmcp service instance.
/// Inbound requests have their JSON-RPC `id` rewritten to the Nostr `event_id`
/// before being forwarded to the rmcp handler.  Since event IDs are globally
/// unique (SHA-256 hashes), this eliminates collisions when different clients
/// use the same JSON-RPC request IDs.  The transport's event-route store
/// handles response routing back to the originating client; server-initiated
/// notifications are broadcast to all initialized clients.
pub struct NostrServerWorker {
    transport: NostrServerTransport,
}

impl NostrServerWorker {
    /// Create a new server worker from existing server transport config.
    pub async fn new<T>(signer: T, config: NostrServerTransportConfig) -> Result<Self>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
    {
        let transport = NostrServerTransport::new(signer, config).await?;
        Ok(Self { transport })
    }

    /// Create a worker from an already-constructed raw transport.
    pub fn from_transport(transport: NostrServerTransport) -> Self {
        Self { transport }
    }

    /// Access the wrapped transport.
    pub fn transport(&self) -> &NostrServerTransport {
        &self.transport
    }
}

impl Worker for NostrServerWorker {
    type Error = crate::core::error::Error;
    type Role = rmcp::RoleServer;

    fn err_closed() -> Self::Error {
        Self::Error::Transport("rmcp worker channel closed".to_string())
    }

    fn err_join(e: tokio::task::JoinError) -> Self::Error {
        Self::Error::Other(format!("rmcp worker join error: {e}"))
    }

    async fn run(
        mut self,
        mut context: WorkerContext<Self>,
    ) -> std::result::Result<(), WorkerQuitReason<Self::Error>> {
        self.transport
            .start()
            .await
            .map_err(WorkerQuitReason::fatal_context("starting server transport"))?;

        let mut rx = self.transport.take_message_receiver().ok_or_else(|| {
            WorkerQuitReason::fatal(
                Self::Error::Other("server message receiver already taken".to_string()),
                "taking server message receiver",
            )
        })?;

        let cancellation_token = context.cancellation_token.clone();

        let quit_reason = loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    break WorkerQuitReason::Cancelled;
                }
                incoming = rx.recv() => {
                    let Some(incoming) = incoming else {
                        break WorkerQuitReason::TransportClosed;
                    };

                    let crate::transport::server::IncomingRequest {
                        mut message,
                        event_id,
                        ..
                    } = incoming;

                    // Rewrite the JSON-RPC request ID to the Nostr event_id.
                    // Event IDs are globally unique (SHA-256), so no collision
                    // across clients.  The transport's event-route store maps
                    // event_id → (client_pubkey, original_request_id) and
                    // restores the original ID in `send_response`.
                    if let JsonRpcMessage::Request(ref mut req) = message {
                        req.id = serde_json::json!(event_id);
                    }

                    if let Some(rmcp_msg) = internal_to_rmcp_server_rx(&message) {
                        if let Err(reason) = context.send_to_handler(rmcp_msg).await {
                            break reason;
                        }
                    } else {
                        tracing::warn!(
                            target: LOG_TARGET,
                            "Failed to convert incoming server-side message to rmcp format"
                        );
                    }
                }
                outbound = context.recv_from_handler() => {
                    let outbound = match outbound {
                        Ok(outbound) => outbound,
                        Err(reason) => break reason,
                    };

                    let result = if let Some(internal_msg) = rmcp_server_tx_to_internal(outbound.message) {
                        self.forward_server_internal(internal_msg).await
                    } else {
                        Err(Self::Error::Validation(
                            "failed converting rmcp server message to internal JSON-RPC".to_string(),
                        ))
                    };

                    let _ = outbound.responder.send(result);
                }
            }
        };

        if let Err(e) = self.transport.close().await {
            tracing::warn!(
                target: LOG_TARGET,
                error = %e,
                "Failed to close server transport cleanly"
            );
        }

        Err(quit_reason)
    }
}

/// rmcp client worker wrapper for ContextVM Nostr client transport.
pub struct NostrClientWorker {
    transport: NostrClientTransport,
}

impl NostrClientWorker {
    /// Create a new client worker from existing client transport config.
    pub async fn new<T>(signer: T, config: NostrClientTransportConfig) -> Result<Self>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
    {
        let transport = NostrClientTransport::new(signer, config).await?;
        Ok(Self { transport })
    }

    /// Create a worker from an already-constructed raw transport.
    pub fn from_transport(transport: NostrClientTransport) -> Self {
        Self { transport }
    }

    /// Access the wrapped transport.
    pub fn transport(&self) -> &NostrClientTransport {
        &self.transport
    }
}

impl Worker for NostrClientWorker {
    type Error = crate::core::error::Error;
    type Role = rmcp::RoleClient;

    fn err_closed() -> Self::Error {
        Self::Error::Transport("rmcp worker channel closed".to_string())
    }

    fn err_join(e: tokio::task::JoinError) -> Self::Error {
        Self::Error::Other(format!("rmcp worker join error: {e}"))
    }

    async fn run(
        mut self,
        mut context: WorkerContext<Self>,
    ) -> std::result::Result<(), WorkerQuitReason<Self::Error>> {
        self.transport
            .start()
            .await
            .map_err(WorkerQuitReason::fatal_context("starting client transport"))?;

        let mut rx = self.transport.take_message_receiver().ok_or_else(|| {
            WorkerQuitReason::fatal(
                Self::Error::Other("client message receiver already taken".to_string()),
                "taking client message receiver",
            )
        })?;

        let cancellation_token = context.cancellation_token.clone();

        let quit_reason = loop {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    break WorkerQuitReason::Cancelled;
                }
                incoming = rx.recv() => {
                    let Some(incoming) = incoming else {
                        break WorkerQuitReason::TransportClosed;
                    };

                    if let Some(rmcp_msg) = internal_to_rmcp_client_rx(&incoming) {
                        if let Err(reason) = context.send_to_handler(rmcp_msg).await {
                            break reason;
                        }
                    } else {
                        tracing::warn!(
                            target: LOG_TARGET,
                            "Failed to convert incoming client-side message to rmcp format"
                        );
                    }
                }
                outbound = context.recv_from_handler() => {
                    let outbound = match outbound {
                        Ok(outbound) => outbound,
                        Err(reason) => break reason,
                    };

                    let result = if let Some(internal_msg) = rmcp_client_tx_to_internal(outbound.message) {
                        self.transport.send(&internal_msg).await
                    } else {
                        Err(Self::Error::Validation(
                            "failed converting rmcp client message to internal JSON-RPC".to_string(),
                        ))
                    };

                    let _ = outbound.responder.send(result);
                }
            }
        };

        if let Err(e) = self.transport.close().await {
            tracing::warn!(
                target: LOG_TARGET,
                error = %e,
                "Failed to close client transport cleanly"
            );
        }

        Err(quit_reason)
    }
}

impl NostrServerWorker {
    /// Forward an outbound message from the rmcp handler to the Nostr transport.
    ///
    /// Response IDs carry the Nostr event_id set during ingest.  The transport's
    /// `send_response` uses this to look up the route (client_pubkey +
    /// original_request_id) and deliver the response to the correct client.
    /// Notifications and server-initiated requests are broadcast to all
    /// initialized clients.
    async fn forward_server_internal(&mut self, message: JsonRpcMessage) -> Result<()> {
        match message {
            JsonRpcMessage::Response(resp) => {
                let event_id = resp.id.as_str().map(str::to_owned).ok_or_else(|| {
                    crate::core::error::Error::Validation(
                        "rmcp server response id is not a string event_id".to_string(),
                    )
                })?;

                self.transport
                    .send_response(&event_id, JsonRpcMessage::Response(resp))
                    .await
            }
            JsonRpcMessage::ErrorResponse(resp) => {
                let event_id = resp.id.as_str().map(str::to_owned).ok_or_else(|| {
                    crate::core::error::Error::Validation(
                        "rmcp server error response id is not a string event_id".to_string(),
                    )
                })?;

                self.transport
                    .send_response(&event_id, JsonRpcMessage::ErrorResponse(resp))
                    .await
            }
            JsonRpcMessage::Notification(notification) => {
                let message = JsonRpcMessage::Notification(notification);
                self.transport.broadcast_notification(&message).await
            }
            JsonRpcMessage::Request(request) => {
                let message = JsonRpcMessage::Request(request);
                self.transport.broadcast_notification(&message).await
            }
        }
    }
}
