//! rmcp worker adapters.
//!
//! This file defines wrapper types that bind existing ContextVM Nostr
//! transports to rmcp's worker abstraction.

use crate::core::constants::ANNOUNCEMENT_REQUEST_ID;
use crate::core::error::Result;
use crate::core::types::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest};
use crate::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use crate::transport::server::{NostrServerTransport, NostrServerTransportConfig};
use rmcp::model::GetExtensions;
use rmcp::transport::worker::{Worker, WorkerContext, WorkerQuitReason};
use std::collections::HashSet;

use super::convert::{
    internal_to_rmcp_client_rx, internal_to_rmcp_server_rx, rmcp_client_tx_to_internal,
    rmcp_server_tx_to_internal,
};

const LOG_TARGET: &str = "contextvm_sdk::rmcp_transport::worker";
const STATELESS_SYNTHETIC_EVENT_ID: &str = "contextvm-stateless-init";

fn synthetic_initialize_message() -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(STATELESS_SYNTHETIC_EVENT_ID),
        method: "initialize".to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": crate::core::constants::mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": {
                "name": "contextvm-stateless-client",
                "version": "0.1.0"
            }
        })),
    })
}

fn synthetic_initialized_notification() -> JsonRpcMessage {
    JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "notifications/initialized".to_string(),
        params: None,
    })
}

fn should_inject_stateless_bootstrap(
    initialized_clients: &HashSet<String>,
    client_pubkey: &str,
    message: &JsonRpcMessage,
) -> bool {
    if initialized_clients.contains(client_pubkey) {
        return false;
    }

    matches!(message, JsonRpcMessage::Request(req) if req.method != "initialize")
}

fn is_synthetic_initialize_message(message: &JsonRpcMessage) -> bool {
    matches!(
        message,
        JsonRpcMessage::Request(req)
            if req.method == "initialize"
                && req.id == serde_json::json!(STATELESS_SYNTHETIC_EVENT_ID)
    )
}

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

        // CEP-6: Spawn auto-publish after start() so the worker's select loop
        // is running when synthetic messages arrive through message_tx.
        self.transport.spawn_announcements();

        let mut rx = self.transport.take_message_receiver().ok_or_else(|| {
            WorkerQuitReason::fatal(
                Self::Error::Other("server message receiver already taken".to_string()),
                "taking server message receiver",
            )
        })?;

        let cancellation_token = context.cancellation_token.clone();
        let mut initialized_clients = HashSet::new();

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
                        client_pubkey,
                        ..
                    } = incoming;

                    let should_inject_bootstrap = should_inject_stateless_bootstrap(
                        &initialized_clients,
                        &client_pubkey,
                        &message,
                    );

                    if should_inject_bootstrap {
                        let synthetic_init = synthetic_initialize_message();
                        let Some(rmcp_init) = internal_to_rmcp_server_rx(&synthetic_init) else {
                            break WorkerQuitReason::fatal(
                                Self::Error::Validation(
                                    "failed converting synthetic initialize request to rmcp format".to_string(),
                                ),
                                "converting synthetic initialize request",
                            );
                        };

                        if let Err(reason) = context.send_to_handler(rmcp_init).await {
                            break reason;
                        }

                        let initialized = synthetic_initialized_notification();
                        let Some(rmcp_initialized) = internal_to_rmcp_server_rx(&initialized) else {
                            break WorkerQuitReason::fatal(
                                Self::Error::Validation(
                                    "failed converting synthetic initialized notification to rmcp format".to_string(),
                                ),
                                "converting synthetic initialized notification",
                            );
                        };

                        if let Err(reason) = context.send_to_handler(rmcp_initialized).await {
                            break reason;
                        }

                        initialized_clients.insert(client_pubkey.clone());
                    }

                    if matches!(&message, JsonRpcMessage::Request(req) if req.method == "initialize")
                        || matches!(&message, JsonRpcMessage::Notification(n) if n.method == "notifications/initialized")
                    {
                        initialized_clients.insert(client_pubkey.clone());
                    }

                    // Rewrite real wire requests to the Nostr event_id.
                    // Synthetic stateless bootstrap messages must retain their
                    // sentinel ID so their responses can be dropped before they
                    // ever touch transport correlation.
                    if !is_synthetic_initialize_message(&message) {
                        if let JsonRpcMessage::Request(ref mut req) = message {
                        req.id = serde_json::json!(event_id);
                        }
                    }

                    if let Some(mut rmcp_msg) = internal_to_rmcp_server_rx(&message) {
                        // CEP-41: inject the open-stream writer into the
                        // request's `extensions` typemap so the tool handler can
                        // reach it via `ctx.extensions.get::<OpenStreamWriter>()`.
                        // The rmcp service loop swaps these extensions straight into
                        // the handler's `RequestContext` before dispatch. No-op when
                        // open-stream is disabled or the request has no writer.
                        if let rmcp::model::JsonRpcMessage::Request(ref mut jr) = rmcp_msg {
                            if let Some(writer) =
                                self.transport.get_open_stream_writer(&event_id)
                            {
                                jr.request.extensions_mut().insert(writer);
                            }
                        }
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

                if event_id == ANNOUNCEMENT_REQUEST_ID {
                    tracing::debug!(
                        target: LOG_TARGET,
                        "Routing announcement response to handler"
                    );
                    return self
                        .transport
                        .handle_announcement_response(JsonRpcMessage::Response(resp))
                        .await;
                }

                if event_id == STATELESS_SYNTHETIC_EVENT_ID {
                    tracing::debug!(
                        target: LOG_TARGET,
                        event_id = %event_id,
                        "Dropping synthetic initialize response before wire transport"
                    );
                    return Ok(());
                }

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

                if event_id == ANNOUNCEMENT_REQUEST_ID {
                    tracing::debug!(
                        target: LOG_TARGET,
                        "Routing announcement error to handler"
                    );
                    return self
                        .transport
                        .handle_announcement_response(JsonRpcMessage::ErrorResponse(resp))
                        .await;
                }

                if event_id == STATELESS_SYNTHETIC_EVENT_ID {
                    tracing::debug!(
                        target: LOG_TARGET,
                        event_id = %event_id,
                        "Dropping synthetic initialize error before wire transport"
                    );
                    return Ok(());
                }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::JsonRpcResponse;

    #[test]
    fn test_should_inject_stateless_bootstrap_for_first_non_initialize_request() {
        let initialized_clients = HashSet::new();
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "tools/list".to_string(),
            params: Some(serde_json::json!({})),
        });

        assert!(should_inject_stateless_bootstrap(
            &initialized_clients,
            "client-a",
            &message,
        ));
    }

    #[test]
    fn test_should_not_inject_stateless_bootstrap_for_real_initialize() {
        let initialized_clients = HashSet::new();
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "initialize".to_string(),
            params: Some(serde_json::json!({})),
        });

        assert!(!should_inject_stateless_bootstrap(
            &initialized_clients,
            "client-a",
            &message,
        ));
    }

    #[test]
    fn test_synthetic_initialize_keeps_sentinel_id() {
        let message = synthetic_initialize_message();

        match message {
            JsonRpcMessage::Request(req) => {
                assert_eq!(req.id, serde_json::json!(STATELESS_SYNTHETIC_EVENT_ID));
                assert_eq!(req.method, "initialize");
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn test_real_request_is_rewritten_to_event_id() {
        let mut message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "tools/list".to_string(),
            params: Some(serde_json::json!({})),
        });

        if let JsonRpcMessage::Request(ref mut req) = message {
            req.id = serde_json::json!("real-event-id");
        }

        match message {
            JsonRpcMessage::Request(req) => {
                assert_eq!(req.id, serde_json::json!("real-event-id"));
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn test_synthetic_initialize_response_uses_sentinel_for_drop() {
        let message = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(STATELESS_SYNTHETIC_EVENT_ID),
            result: serde_json::json!({}),
        });

        match message {
            JsonRpcMessage::Response(resp) => {
                assert_eq!(resp.id.as_str(), Some(STATELESS_SYNTHETIC_EVENT_ID));
            }
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[test]
    fn test_synthetic_initialized_notification_shape() {
        let message = synthetic_initialized_notification();
        match message {
            JsonRpcMessage::Notification(notification) => {
                assert_eq!(notification.method, "notifications/initialized");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn test_is_synthetic_initialize_message_detects_sentinel() {
        assert!(is_synthetic_initialize_message(
            &synthetic_initialize_message()
        ));
    }

    #[test]
    fn test_announcement_sentinel_differs_from_stateless_sentinel() {
        assert_ne!(ANNOUNCEMENT_REQUEST_ID, STATELESS_SYNTHETIC_EVENT_ID);
    }

    #[test]
    fn test_announcement_response_id_detected() {
        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": { "name": "test" }
            }),
        });

        if let JsonRpcMessage::Response(ref resp) = response {
            let event_id = resp.id.as_str().unwrap();
            assert_eq!(event_id, ANNOUNCEMENT_REQUEST_ID);
            // Must not be confused with the stateless synthetic sentinel
            assert!(!is_synthetic_initialize_message(&response));
        }
    }
}
