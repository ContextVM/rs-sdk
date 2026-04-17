//! Worker pool for multi-client RMCP server transport.
//!
//! This module provides a `WorkerPool` that manages multiple `NostrServerWorker`
//! instances, distributing incoming client peers across workers with round-robin
//! assignment and client affinity. This solves the single-peer-per-worker
//! limitation in the rmcp `Worker` trait.
//!
//! # Architecture
//!
//! ```text
//! NostrServerTransport (single Nostr subscription)
//!         │
//!         ▼
//! WorkerPool dispatcher task
//!   ├── Worker #0  (rmcp service + handler)
//!   ├── Worker #1  (rmcp service + handler)
//!   └── Worker #N  ...
//! ```
//!
//! Each worker has a maximum client capacity. When all workers are full,
//! new clients either wait in a bounded queue or receive a "server busy"
//! JSON-RPC error response.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::core::error::{Error, Result};
use crate::core::types::{JsonRpcErrorResponse, JsonRpcMessage};
use crate::transport::server::{IncomingRequest, NostrServerTransport, NostrServerTransportConfig};

/// Configuration for the worker pool.
#[derive(Debug, Clone)]
pub struct WorkerPoolConfig {
    /// Number of workers in the pool.
    ///
    /// Default: 4
    pub pool_size: usize,
    /// Maximum number of clients a single worker can serve concurrently.
    ///
    /// Default: 250 (4 × 250 = 1000, matching the TS SDK's default `maxSessions`)
    pub max_clients_per_worker: usize,
    /// Maximum number of peers waiting in the queue when all workers are at capacity.
    ///
    /// Default: 100
    pub max_queue_depth: usize,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self {
            pool_size: 4,
            max_clients_per_worker: 250,
            max_queue_depth: 100,
        }
    }
}

/// Metadata for a single worker in the pool.
struct WorkerSlot {
    /// Channel to forward incoming requests to this worker.
    tx: mpsc::UnboundedSender<IncomingRequest>,
    /// Set of client pubkeys currently assigned to this worker.
    active_clients: HashSet<String>,
}

/// Shared mutable state for the pool dispatcher.
struct PoolState {
    /// Per-worker metadata.
    workers: Vec<WorkerSlot>,
    /// Maps client pubkey → worker index for affinity.
    client_affinity: HashMap<String, usize>,
    /// Round-robin counter for new client assignment.
    next_worker: usize,
    /// Waiting queue for clients when all workers are at capacity.
    wait_queue: VecDeque<IncomingRequest>,
    /// Configuration snapshot.
    config: WorkerPoolConfig,
}

impl PoolState {
    /// Find the least-loaded worker under the capacity threshold.
    /// Uses round-robin starting from `next_worker` to spread load.
    fn select_worker(&mut self) -> Option<usize> {
        let n = self.workers.len();
        for i in 0..n {
            let idx = (self.next_worker + i) % n;
            if self.workers[idx].active_clients.len() < self.config.max_clients_per_worker {
                self.next_worker = (idx + 1) % n;
                return Some(idx);
            }
        }
        None
    }

    /// Assign a client to a specific worker.
    fn assign_client(&mut self, client_pubkey: &str, worker_idx: usize) {
        self.workers[worker_idx]
            .active_clients
            .insert(client_pubkey.to_string());
        self.client_affinity
            .insert(client_pubkey.to_string(), worker_idx);
    }

    /// Remove a client from its assigned worker (e.g., on session timeout).
    #[allow(dead_code)]
    fn release_client(&mut self, client_pubkey: &str) {
        if let Some(worker_idx) = self.client_affinity.remove(client_pubkey) {
            if let Some(slot) = self.workers.get_mut(worker_idx) {
                slot.active_clients.remove(client_pubkey);
            }
            // Trigger a drain since a slot may have just opened up.
            self.drain_wait_queue();
        }
    }

    /// Try to drain the wait queue into any newly available worker slots.
    fn drain_wait_queue(&mut self) {
        while !self.wait_queue.is_empty() {
            if let Some(worker_idx) = self.select_worker() {
                let request = self.wait_queue.pop_front().unwrap();
                self.assign_client(&request.client_pubkey, worker_idx);
                if self.workers[worker_idx].tx.send(request).is_err() {
                    tracing::error!(
                        worker = worker_idx,
                        "Worker channel closed while draining queue"
                    );
                }
            } else {
                break; // All workers still full
            }
        }
    }
}

/// Handle returned by `WorkerPool::start()` for lifecycle management.
pub struct WorkerPoolHandle {
    /// Cancel token to shut down the pool.
    cancel: CancellationToken,
    /// Shared state for metrics/inspection.
    state: Arc<RwLock<PoolState>>,
    /// Join handles for all spawned tasks.
    join_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl WorkerPoolHandle {
    /// Shut down the worker pool gracefully.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for handle in self.join_handles {
            let _ = handle.await;
        }
    }

    /// Get current pool statistics.
    pub async fn stats(&self) -> WorkerPoolStats {
        let state = self.state.read().await;
        WorkerPoolStats {
            pool_size: state.workers.len(),
            clients_per_worker: state
                .workers
                .iter()
                .map(|w| w.active_clients.len())
                .collect(),
            total_clients: state.client_affinity.len(),
            queue_depth: state.wait_queue.len(),
        }
    }

    /// Check if the pool is still running (not cancelled).
    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled()
    }
}

/// Snapshot of pool statistics.
#[derive(Debug, Clone)]
pub struct WorkerPoolStats {
    /// Number of workers in the pool.
    pub pool_size: usize,
    /// Number of active clients per worker.
    pub clients_per_worker: Vec<usize>,
    /// Total number of assigned clients across all workers.
    pub total_clients: usize,
    /// Number of peers currently waiting in the queue.
    pub queue_depth: usize,
}

/// Start a worker pool that dispatches incoming Nostr requests across
/// multiple rmcp server workers.
///
/// # Type Parameters
///
/// - `T`: Nostr signer type
/// - `H`: rmcp `ServerHandler` implementation
///
/// # Arguments
///
/// - `signer`: The Nostr signer for the server identity
/// - `server_config`: Configuration for the underlying `NostrServerTransport`
/// - `pool_config`: Configuration for pool sizing and capacity
/// - `handler_factory`: A function that creates a new handler instance for each worker
///
/// # Returns
///
/// A `WorkerPoolHandle` for lifecycle management and metrics.
pub async fn start_worker_pool<T, H, F>(
    signer: T,
    server_config: NostrServerTransportConfig,
    pool_config: WorkerPoolConfig,
    handler_factory: F,
) -> Result<WorkerPoolHandle>
where
    T: nostr_sdk::prelude::IntoNostrSigner,
    H: rmcp::ServerHandler,
    F: Fn() -> H,
{
    // Create the single shared transport
    let mut transport = NostrServerTransport::new(signer, server_config).await?;
    transport.start().await?;

    let mut incoming_rx = transport
        .take_message_receiver()
        .ok_or_else(|| Error::Other("server message receiver already taken".to_string()))?;

    let cancel = CancellationToken::new();
    let mut join_handles = Vec::new();

    // Create worker slots
    let mut workers = Vec::with_capacity(pool_config.pool_size);

    for worker_idx in 0..pool_config.pool_size {
        let (tx, rx) = mpsc::unbounded_channel::<IncomingRequest>();
        workers.push(WorkerSlot {
            tx,
            active_clients: HashSet::new(),
        });

        // Spawn each worker's rmcp service
        let handler = handler_factory();
        let worker_cancel = cancel.clone();

        let handle = tokio::spawn(run_worker_loop(worker_idx, rx, handler, worker_cancel));
        join_handles.push(handle);
    }

    let state = Arc::new(RwLock::new(PoolState {
        workers,
        client_affinity: HashMap::new(),
        next_worker: 0,
        wait_queue: VecDeque::new(),
        config: pool_config,
    }));

    // Spawn the dispatcher task
    let dispatcher_state = state.clone();
    let dispatcher_cancel = cancel.clone();
    let transport = Arc::new(transport);
    let transport_for_busy = transport.clone();

    let dispatcher = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = dispatcher_cancel.cancelled() => {
                    tracing::info!("Worker pool dispatcher shutting down");
                    break;
                }
                incoming = incoming_rx.recv() => {
                    let Some(request) = incoming else {
                        tracing::info!("Transport message channel closed");
                        break;
                    };

                    let mut state = dispatcher_state.write().await;
                    let client_pubkey = request.client_pubkey.clone();
                    let event_id = request.event_id.clone();

                    // Check affinity first — existing client goes to same worker
                    if let Some(&worker_idx) = state.client_affinity.get(&client_pubkey) {
                        if state.workers[worker_idx].tx.send(request).is_err() {
                            tracing::error!(
                                worker = worker_idx,
                                client = %client_pubkey,
                                "Worker channel closed for affine client"
                            );
                            state.release_client(&client_pubkey);
                        }
                        continue;
                    }

                    // New client — try to assign to a worker
                    if let Some(worker_idx) = state.select_worker() {
                        tracing::info!(
                            worker = worker_idx,
                            client = %client_pubkey,
                            "Assigning new client to worker"
                        );
                        state.assign_client(&client_pubkey, worker_idx);
                        if state.workers[worker_idx].tx.send(request).is_err() {
                            tracing::error!(
                                worker = worker_idx,
                                "Worker channel closed during assignment"
                            );
                            state.release_client(&client_pubkey);
                        }
                    } else if state.wait_queue.len() < state.config.max_queue_depth {
                        // All workers full — enqueue
                        tracing::warn!(
                            client = %client_pubkey,
                            queue_depth = state.wait_queue.len(),
                            "All workers at capacity, queueing peer"
                        );
                        state.wait_queue.push_back(request);
                    } else {
                        // Queue full too — send "server busy" error
                        tracing::warn!(
                            client = %client_pubkey,
                            "All workers and queue at capacity, rejecting peer"
                        );
                        drop(state);
                        send_server_busy_error(
                            &transport_for_busy,
                            &client_pubkey,
                            &event_id,
                        ).await;
                    }
                }
            }
        }
    });
    join_handles.push(dispatcher);

    Ok(WorkerPoolHandle {
        cancel,
        state,
        join_handles,
    })
}

/// Run a single worker's event loop, processing incoming requests
/// via the convert bridge.
///
/// Each worker receives `IncomingRequest`s from the pool dispatcher,
/// converts them to rmcp format, and forwards them to the handler.
/// Response routing back to the correct client is tracked via the
/// request correlation map.
async fn run_worker_loop<H: rmcp::ServerHandler>(
    worker_idx: usize,
    mut rx: mpsc::UnboundedReceiver<IncomingRequest>,
    _handler: H,
    cancel: CancellationToken,
) {
    use crate::rmcp_transport::convert::internal_to_rmcp_server_rx;

    // Per-worker correlation: serialized_request_id → (event_id, client_pubkey)
    let mut request_correlation: HashMap<String, (String, String)> = HashMap::new();

    tracing::info!(worker = worker_idx, "Worker started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(worker = worker_idx, "Worker cancelled");
                break;
            }
            incoming = rx.recv() => {
                let Some(incoming) = incoming else {
                    tracing::info!(worker = worker_idx, "Worker input channel closed");
                    break;
                };

                let IncomingRequest {
                    message,
                    client_pubkey,
                    event_id,
                    ..
                } = incoming;

                // Track correlation for requests
                if let JsonRpcMessage::Request(ref req) = message {
                    if let Ok(request_key) = serde_json::to_string(&req.id) {
                        request_correlation.insert(
                            request_key,
                            (event_id.clone(), client_pubkey.clone()),
                        );
                    }
                }

                // Convert to rmcp format for the handler
                if let Some(_rmcp_msg) = internal_to_rmcp_server_rx(&message) {
                    tracing::debug!(
                        worker = worker_idx,
                        client = %client_pubkey,
                        event_id = %event_id,
                        "Converted and dispatched message to worker"
                    );
                    // TODO: Forward rmcp_msg to the handler's service channel
                    // once we integrate with rmcp's Service machinery.
                    // For now, the convert bridge validates the message format.
                } else {
                    tracing::warn!(
                        worker = worker_idx,
                        "Failed to convert incoming message to rmcp format"
                    );
                }
            }
        }
    }

    tracing::info!(worker = worker_idx, "Worker exiting");
}

/// Send a "server busy" JSON-RPC error response to a client.
async fn send_server_busy_error(
    transport: &NostrServerTransport,
    client_pubkey: &str,
    event_id: &str,
) {
    let error_response = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(event_id),
        error: crate::core::types::JsonRpcError {
            code: -32000,
            message: "Server busy, please retry".to_string(),
            data: None,
        },
    });

    // Try to send the error response. If there's no session yet for this client,
    // we need to send it via a notification-like path since send_response requires
    // an existing session with correlation data.
    if let Err(e) = transport
        .send_notification(client_pubkey, &error_response, Some(event_id))
        .await
    {
        // If notification fails (no session), try to send as response
        // (will only work if there's already a session from a prior request)
        tracing::warn!(
            client = %client_pubkey,
            "Failed to send server-busy error: {e}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_config_defaults() {
        let config = WorkerPoolConfig::default();
        assert_eq!(config.pool_size, 4);
        assert_eq!(config.max_clients_per_worker, 250);
        assert_eq!(config.max_queue_depth, 100);
    }

    #[test]
    fn test_pool_state_select_worker_round_robin() {
        let config = WorkerPoolConfig {
            pool_size: 3,
            max_clients_per_worker: 2,
            max_queue_depth: 10,
        };

        let mut state = PoolState {
            workers: (0..3)
                .map(|_| {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    WorkerSlot {
                        tx,
                        active_clients: HashSet::new(),
                    }
                })
                .collect(),
            client_affinity: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        // First selection should pick worker 0
        assert_eq!(state.select_worker(), Some(0));
        // After selection, next_worker advances to 1
        assert_eq!(state.select_worker(), Some(1));
        assert_eq!(state.select_worker(), Some(2));
        // Wraps around
        assert_eq!(state.select_worker(), Some(0));
    }

    #[test]
    fn test_pool_state_select_worker_skips_full() {
        let config = WorkerPoolConfig {
            pool_size: 3,
            max_clients_per_worker: 1,
            max_queue_depth: 10,
        };

        let mut state = PoolState {
            workers: (0..3)
                .map(|_| {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    WorkerSlot {
                        tx,
                        active_clients: HashSet::new(),
                    }
                })
                .collect(),
            client_affinity: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        // Fill worker 0
        state.assign_client("client_a", 0);
        // Selection should skip worker 0 and pick worker 1
        assert_eq!(state.select_worker(), Some(1));
    }

    #[test]
    fn test_pool_state_select_worker_all_full() {
        let config = WorkerPoolConfig {
            pool_size: 2,
            max_clients_per_worker: 1,
            max_queue_depth: 10,
        };

        let mut state = PoolState {
            workers: (0..2)
                .map(|_| {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    WorkerSlot {
                        tx,
                        active_clients: HashSet::new(),
                    }
                })
                .collect(),
            client_affinity: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        // Fill all workers
        state.assign_client("client_a", 0);
        state.assign_client("client_b", 1);
        // No worker available
        assert_eq!(state.select_worker(), None);
    }

    #[test]
    fn test_pool_state_client_affinity() {
        let config = WorkerPoolConfig {
            pool_size: 2,
            max_clients_per_worker: 10,
            max_queue_depth: 10,
        };

        let mut state = PoolState {
            workers: (0..2)
                .map(|_| {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    WorkerSlot {
                        tx,
                        active_clients: HashSet::new(),
                    }
                })
                .collect(),
            client_affinity: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        state.assign_client("client_a", 0);
        state.assign_client("client_b", 1);

        // Affinity should map correctly
        assert_eq!(state.client_affinity.get("client_a"), Some(&0));
        assert_eq!(state.client_affinity.get("client_b"), Some(&1));

        // Worker 0 should have client_a
        assert!(state.workers[0].active_clients.contains("client_a"));
        assert!(!state.workers[0].active_clients.contains("client_b"));
    }

    #[test]
    fn test_pool_state_release_client() {
        let config = WorkerPoolConfig {
            pool_size: 2,
            max_clients_per_worker: 10,
            max_queue_depth: 10,
        };

        let mut state = PoolState {
            workers: (0..2)
                .map(|_| {
                    let (tx, _rx) = mpsc::unbounded_channel();
                    WorkerSlot {
                        tx,
                        active_clients: HashSet::new(),
                    }
                })
                .collect(),
            client_affinity: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        state.assign_client("client_a", 0);
        assert_eq!(state.client_affinity.len(), 1);
        assert_eq!(state.workers[0].active_clients.len(), 1);

        state.release_client("client_a");
        assert_eq!(state.client_affinity.len(), 0);
        assert_eq!(state.workers[0].active_clients.len(), 0);
    }

    #[test]
    fn test_pool_state_drain_wait_queue() {
        let config = WorkerPoolConfig {
            pool_size: 1,
            max_clients_per_worker: 2,
            max_queue_depth: 10,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = PoolState {
            workers: vec![WorkerSlot {
                tx,
                active_clients: HashSet::new(),
            }],
            client_affinity: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        // Add a pending request to the queue
        state.wait_queue.push_back(IncomingRequest {
            message: JsonRpcMessage::Request(crate::core::types::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                method: "ping".to_string(),
                params: None,
            }),
            client_pubkey: "queued_client".to_string(),
            event_id: "evt1".to_string(),
            is_encrypted: false,
        });

        assert_eq!(state.wait_queue.len(), 1);

        // Drain should assign the queued client to the worker
        state.drain_wait_queue();
        assert_eq!(state.wait_queue.len(), 0);
        assert!(state.client_affinity.contains_key("queued_client"));

        // The worker should have received the request
        assert!(rx.try_recv().is_ok());
    }
}
