//! rmcp worker adapters.
//!
//! This file defines wrapper types that bind existing ContextVM Nostr
//! transports to rmcp's worker abstraction.
//!
//! # Deprecation
//!
//! [`NostrServerWorker`] is deprecated. Use [`super::pool::start_worker_pool`]
//! for multi-client server deployments.

use std::collections::HashMap;

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
/// # Deprecated
///
/// This worker accepts messages from **all** connected clients but routes
/// responses via its internal correlation map.  For production deployments
/// that need capacity management and backpressure, use
/// [`super::pool::start_worker_pool`] instead.
#[deprecated(
    since = "0.2.0",
    note = "Use `rmcp_transport::pool::start_worker_pool` for multi-client support with capacity management"
)]
pub struct NostrServerWorker {
    transport: NostrServerTransport,
    /// Multi-client correlation: serialized_request_id → (event_id, client_pubkey).
    request_id_to_event_id: HashMap<String, String>,
    /// Tracks which client pubkey is associated with each request for response routing.
    request_id_to_client: HashMap<String, String>,
}

#[allow(deprecated)]
impl NostrServerWorker {
    /// Create a new server worker from existing server transport config.
    pub async fn new<T>(signer: T, config: NostrServerTransportConfig) -> Result<Self>
    where
        T: nostr_sdk::prelude::IntoNostrSigner,
    {
        let transport = NostrServerTransport::new(signer, config).await?;
        Ok(Self {
            transport,
            request_id_to_event_id: HashMap::new(),
            request_id_to_client: HashMap::new(),
        })
    }

    /// Access the wrapped transport.
    pub fn transport(&self) -> &NostrServerTransport {
        &self.transport
    }
}

#[allow(deprecated)]
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
                        message,
                        client_pubkey,
                        event_id,
                        ..
                    } = incoming;

                    // Accept messages from any client (multi-client support).
                    // Track correlation for response routing.

                    if let JsonRpcMessage::Request(req) = &message {
                        match serde_json::to_string(&req.id) {
                            Ok(request_key) => {
                                self.request_id_to_event_id.insert(request_key.clone(), event_id);
                                self.request_id_to_client.insert(request_key, client_pubkey);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: LOG_TARGET,
                                    error = %e,
                                    "Failed to serialize request id for correlation map"
                                );
                            }
                        }
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

#[allow(deprecated)]
impl NostrServerWorker {
    async fn forward_server_internal(&mut self, message: JsonRpcMessage) -> Result<()> {
        match message {
            JsonRpcMessage::Response(resp) => {
                let request_key = serde_json::to_string(&resp.id).map_err(|e| {
                    crate::core::error::Error::Validation(format!(
                        "failed to serialize rmcp response id for correlation lookup: {e}"
                    ))
                })?;

                let event_id =
                    if let Some(event_id) = self.request_id_to_event_id.remove(&request_key) {
                        event_id
                    } else {
                        resp.id.as_str().map(str::to_owned).ok_or_else(|| {
                            crate::core::error::Error::Validation(
							"rmcp server response id has no known correlation mapping and is not a string event id"
								.to_string(),
						)
                        })?
                    };

                self.transport
                    .send_response(&event_id, JsonRpcMessage::Response(resp))
                    .await
            }
            JsonRpcMessage::ErrorResponse(resp) => {
                let request_key = serde_json::to_string(&resp.id).map_err(|e| {
                    crate::core::error::Error::Validation(format!(
                        "failed to serialize rmcp error response id for correlation lookup: {e}"
                    ))
                })?;

                let event_id =
                    if let Some(event_id) = self.request_id_to_event_id.remove(&request_key) {
                        event_id
                    } else {
                        resp.id.as_str().map(str::to_owned).ok_or_else(|| {
                            crate::core::error::Error::Validation(
							"rmcp server error response id has no known correlation mapping and is not a string event id"
								.to_string(),
						)
                        })?
                    };

                self.transport
                    .send_response(&event_id, JsonRpcMessage::ErrorResponse(resp))
                    .await
            }
            JsonRpcMessage::Notification(notification) => {
                // For notifications, try to find a recent client from the correlation map.
                // Fall back to broadcasting if no specific client is known.
                let target = self.request_id_to_client.values().next().cloned();
                match target {
                    Some(pubkey) => {
                        let message = JsonRpcMessage::Notification(notification);
                        self.transport
                            .send_notification(&pubkey, &message, None)
                            .await
                    }
                    None => {
                        let message = JsonRpcMessage::Notification(notification);
                        self.transport.broadcast_notification(&message).await
                    }
                }
            }
            JsonRpcMessage::Request(request) => {
                // Server-to-client request: route to a recent client.
                let target = self.request_id_to_client.values().next().cloned();
                match target {
                    Some(pubkey) => {
                        let message = JsonRpcMessage::Request(request);
                        self.transport
                            .send_notification(&pubkey, &message, None)
                            .await
                    }
                    None => Err(crate::core::error::Error::Validation(
                        "cannot forward rmcp server request: no clients connected".to_string(),
                    )),
                }
            }
        }
    }
}
