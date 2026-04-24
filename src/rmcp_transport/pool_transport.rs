//! Custom rmcp transport for pool workers.
//!
//! [`PoolWorkerTransport`] implements `rmcp::transport::Transport<RoleServer>` so
//! that each per-client service within a pool worker can be driven by rmcp's
//! standard `Service` machinery. This avoids manual handler dispatch and gives
//! us the exact same execution path as `NostrServerWorker::run()`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::core::error::Error;
use crate::core::types::JsonRpcMessage;
use crate::transport::server::{IncomingRequest, NostrServerTransport};

use super::convert::{internal_to_rmcp_server_rx, rmcp_server_tx_to_internal};

/// Transport adapter that bridges the pool dispatcher channel to a single
/// rmcp service instance, handling one client's messages.
///
/// # Lifetime constraints
///
/// The rmcp `Transport` trait requires `send()` to return a `'static` future,
/// meaning the future cannot borrow `self`. We solve this by keeping mutable
/// state behind `Arc<Mutex<_>>` and cloning Arcs into each future.
pub struct PoolWorkerTransport {
    /// Receives incoming requests for this specific client.
    incoming_rx: mpsc::UnboundedReceiver<IncomingRequest>,
    /// Shared server transport for sending responses back over Nostr.
    server_transport: Arc<NostrServerTransport>,
    /// Correlation: serialized JSON-RPC request id → (nostr_event_id, client_pubkey).
    /// Behind Arc<Mutex> so send() futures can be 'static.
    request_correlation: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// The client pubkey this transport is serving.
    client_pubkey: String,
    /// Cancellation token for graceful shutdown.
    cancel: CancellationToken,
}

impl PoolWorkerTransport {
    /// Create a new pool worker transport for a specific client.
    pub fn new(
        incoming_rx: mpsc::UnboundedReceiver<IncomingRequest>,
        server_transport: Arc<NostrServerTransport>,
        client_pubkey: String,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            incoming_rx,
            server_transport,
            request_correlation: Arc::new(Mutex::new(HashMap::new())),
            client_pubkey,
            cancel,
        }
    }
}

impl rmcp::transport::Transport<rmcp::RoleServer> for PoolWorkerTransport {
    type Error = Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        // Clone everything we need so the future is 'static (no &self borrow).
        let server_transport = self.server_transport.clone();
        let correlation = self.request_correlation.clone();
        let client_pubkey = self.client_pubkey.clone();

        async move {
            // Convert rmcp server outbound → internal JSON-RPC
            let internal_msg = rmcp_server_tx_to_internal(item).ok_or_else(|| {
                Error::Validation(
                    "failed converting rmcp server message to internal JSON-RPC".to_string(),
                )
            })?;

            // Extract correlation info before moving internal_msg.
            let response_id = match &internal_msg {
                JsonRpcMessage::Response(resp) => Some(resp.id.clone()),
                JsonRpcMessage::ErrorResponse(resp) => Some(resp.id.clone()),
                _ => None,
            };

            if let Some(id) = response_id {
                let request_key = serde_json::to_string(&id).map_err(|e| {
                    Error::Validation(format!("failed to serialize response id: {e}"))
                })?;

                let event_id = {
                    let mut corr = correlation.lock().await;
                    if let Some((eid, _)) = corr.remove(&request_key) {
                        eid
                    } else if let Some(eid) = id.as_str() {
                        eid.to_owned()
                    } else {
                        return Err(Error::Validation(
                            "response id has no correlation mapping and is not a string event id"
                                .to_string(),
                        ));
                    }
                };

                server_transport
                    .send_response(&event_id, internal_msg)
                    .await
            } else {
                // Notification or server→client request
                server_transport
                    .send_notification(&client_pubkey, &internal_msg, None)
                    .await
            }
        }
    }

    async fn receive(&mut self) -> Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>> {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    return None;
                }
                incoming = self.incoming_rx.recv() => {
                    let incoming = incoming?;

                    let IncomingRequest {
                        message,
                        client_pubkey,
                        event_id,
                        ..
                    } = incoming;

                    // Track correlation for response routing
                    if let JsonRpcMessage::Request(ref req) = message {
                        if let Ok(request_key) = serde_json::to_string(&req.id) {
                            self.request_correlation.lock().await.insert(
                                request_key,
                                (event_id.clone(), client_pubkey.clone()),
                            );
                        }
                    }

                    // Convert to rmcp format
                    if let Some(rmcp_msg) = internal_to_rmcp_server_rx(&message) {
                        return Some(rmcp_msg);
                    }

                    tracing::warn!(
                        client = %client_pubkey,
                        "Failed to convert incoming message to rmcp format, skipping"
                    );
                    // Continue loop to try next message
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        tracing::debug!(client = %self.client_pubkey, "Pool worker transport closing");
        Ok(())
    }
}
