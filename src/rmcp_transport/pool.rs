//! Worker pool for multi-client RMCP server transport.
//!
//! This module provides a `WorkerPool` that manages multiple workers,
//! distributing incoming client peers across workers with round-robin
//! assignment and client affinity. Each client gets its own rmcp `Service`
//! instance (via `handler.serve(pool_transport)`) matching the TS SDK's
//! per-client transport model.
//!
//! # Architecture
//!
//! ```text
//! NostrServerTransport (single Nostr subscription)
//!         │
//!         ▼
//! WorkerPool dispatcher task
//!   ├── Worker #0
//!   │   ├── Client A  (handler.serve(transport_a))
//!   │   └── Client B  (handler.serve(transport_b))
//!   ├── Worker #1
//!   │   └── Client C  (handler.serve(transport_c))
//!   └── Worker #N  ...
//! ```
//!
//! Each worker manages up to `max_clients_per_worker` concurrent client
//! sessions. When all workers are full, new clients wait in a bounded queue
//! or receive a "-32000 Server busy" JSON-RPC error response.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use crate::core::error::{Error, Result};
use crate::core::types::{JsonRpcErrorResponse, JsonRpcMessage};
use crate::transport::server::{IncomingRequest, NostrServerTransport, NostrServerTransportConfig};

use super::pool_transport::PoolWorkerTransport;

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
    /// Channel to forward incoming requests to this worker's dispatcher.
    tx: mpsc::UnboundedSender<IncomingRequest>,
    /// Set of client pubkeys currently assigned to this worker.
    active_clients: HashSet<String>,
}

/// Shared mutable state for the pool dispatcher.
pub(crate) struct PoolState {
    /// Per-worker metadata.
    workers: Vec<WorkerSlot>,
    /// Maps client pubkey → worker index for affinity.
    client_affinity: HashMap<String, usize>,
    /// Maps client pubkey → per-client request channel.
    /// Each client has its own channel feeding its dedicated rmcp Service.
    client_channels: HashMap<String, mpsc::UnboundedSender<IncomingRequest>>,
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

    /// Remove a client from its assigned worker (e.g., on session timeout/eviction).
    pub(crate) fn release_client(&mut self, client_pubkey: &str) {
        if let Some(worker_idx) = self.client_affinity.remove(client_pubkey) {
            if let Some(slot) = self.workers.get_mut(worker_idx) {
                slot.active_clients.remove(client_pubkey);
            }
            // Remove the per-client channel (dropping it closes the transport)
            self.client_channels.remove(client_pubkey);
            // Try to drain the wait queue since a slot opened up
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

/// Handle returned by [`start_worker_pool`] for lifecycle management.
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
/// Each client assigned to a worker gets its own rmcp `Service` instance,
/// created by calling `handler_factory()` and `handler.serve(pool_transport)`.
/// This matches the TS SDK's per-client transport model.
///
/// # Type Parameters
///
/// - `T`: Nostr signer type
/// - `H`: rmcp `ServerHandler` implementation
/// - `F`: Handler factory function
///
/// # Arguments
///
/// - `signer`: The Nostr signer for the server identity
/// - `server_config`: Configuration for the underlying `NostrServerTransport`
/// - `pool_config`: Configuration for pool sizing and capacity
/// - `handler_factory`: A function that creates a new handler instance for each client
///
/// # Returns
///
/// A `WorkerPoolHandle` for lifecycle management and metrics.
pub async fn start_worker_pool<T, H, F>(
    signer: T,
    mut server_config: NostrServerTransportConfig,
    pool_config: WorkerPoolConfig,
    handler_factory: F,
) -> Result<WorkerPoolHandle>
where
    T: nostr_sdk::prelude::IntoNostrSigner,
    H: rmcp::ServerHandler,
    F: Fn() -> H + Send + Sync + 'static,
{
    // Create the session eviction channel so cleanup_sessions can notify us
    let (eviction_tx, mut eviction_rx) = mpsc::unbounded_channel::<String>();
    server_config.session_eviction_tx = Some(eviction_tx);

    // Create the single shared transport
    let mut transport = NostrServerTransport::new(signer, server_config).await?;
    transport.start().await?;

    let mut incoming_rx = transport
        .take_message_receiver()
        .ok_or_else(|| Error::Other("server message receiver already taken".to_string()))?;

    let cancel = CancellationToken::new();
    let mut join_handles = Vec::new();

    let transport = Arc::new(transport);
    let handler_factory = Arc::new(handler_factory);

    // Create worker slots
    let mut workers = Vec::with_capacity(pool_config.pool_size);

    for _worker_idx in 0..pool_config.pool_size {
        let (tx, _rx) = mpsc::unbounded_channel::<IncomingRequest>();
        workers.push(WorkerSlot {
            tx,
            active_clients: HashSet::new(),
        });
        // Note: worker tasks are not pre-spawned. Per-client service tasks
        // are spawned on demand by the dispatcher when new clients arrive.
    }

    let state = Arc::new(RwLock::new(PoolState {
        workers,
        client_affinity: HashMap::new(),
        client_channels: HashMap::new(),
        next_worker: 0,
        wait_queue: VecDeque::new(),
        config: pool_config,
    }));

    // Spawn the dispatcher task
    let dispatcher_state = state.clone();
    let dispatcher_cancel = cancel.clone();
    let transport_for_dispatcher = transport.clone();
    let handler_factory_for_dispatcher = handler_factory.clone();

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

                    // Check if this client already has a channel (affinity)
                    if let Some(client_tx) = state.client_channels.get(&client_pubkey) {
                        if client_tx.send(request).is_err() {
                            tracing::warn!(
                                client = %client_pubkey,
                                "Client channel closed, releasing"
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

                        // Create a per-client channel and spawn a service task
                        let (client_tx, client_rx) =
                            mpsc::unbounded_channel::<IncomingRequest>();
                        state
                            .client_channels
                            .insert(client_pubkey.clone(), client_tx.clone());

                        // Send the first message
                        let _ = client_tx.send(request);

                        // Spawn a per-client rmcp service task
                        let handler = handler_factory_for_dispatcher();
                        let client_cancel = dispatcher_cancel.child_token();
                        let client_transport = transport_for_dispatcher.clone();
                        let client_pubkey_owned = client_pubkey.clone();
                        let cleanup_state = dispatcher_state.clone();

                        tokio::spawn(async move {
                            run_client_service(
                                handler,
                                client_rx,
                                client_transport,
                                client_pubkey_owned.clone(),
                                client_cancel,
                            )
                            .await;

                            // When the service exits, release the client slot
                            let mut state = cleanup_state.write().await;
                            state.release_client(&client_pubkey_owned);
                            tracing::debug!(
                                client = %client_pubkey_owned,
                                "Client service exited, slot released"
                            );
                        });
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
                            &transport_for_dispatcher,
                            &client_pubkey,
                            &event_id,
                        )
                        .await;
                    }
                }
            }
        }
    });
    join_handles.push(dispatcher);

    // Spawn the eviction listener task
    let eviction_state = state.clone();
    let eviction_cancel = cancel.clone();

    let eviction_listener = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = eviction_cancel.cancelled() => {
                    break;
                }
                evicted = eviction_rx.recv() => {
                    let Some(pubkey) = evicted else {
                        break;
                    };
                    let mut state = eviction_state.write().await;
                    if state.client_affinity.contains_key(&pubkey) {
                        tracing::info!(
                            client = %pubkey,
                            "Session evicted by transport cleanup, releasing pool slot"
                        );
                        state.release_client(&pubkey);
                    }
                }
            }
        }
    });
    join_handles.push(eviction_listener);

    Ok(WorkerPoolHandle {
        cancel,
        state,
        join_handles,
    })
}

/// Run a per-client rmcp service instance.
///
/// Creates a `PoolWorkerTransport` and calls `handler.serve(transport)`,
/// letting rmcp's `Service` machinery drive the handler automatically.
async fn run_client_service<H: rmcp::ServerHandler>(
    handler: H,
    incoming_rx: mpsc::UnboundedReceiver<IncomingRequest>,
    server_transport: Arc<NostrServerTransport>,
    client_pubkey: String,
    cancel: CancellationToken,
) {
    use rmcp::ServiceExt;

    let pool_transport = PoolWorkerTransport::new(
        incoming_rx,
        server_transport,
        client_pubkey.clone(),
        cancel,
    );

    match handler.serve(pool_transport).await {
        Ok(running_service) => {
            tracing::debug!(client = %client_pubkey, "rmcp service started for client");
            // Block until the service completes (client disconnects or cancel)
            match running_service.waiting().await {
                Ok(quit_reason) => {
                    tracing::debug!(
                        client = %client_pubkey,
                        reason = ?quit_reason,
                        "rmcp service completed"
                    );
                }
                Err(e) => {
                    tracing::debug!(client = %client_pubkey, error = %e, "rmcp service join error");
                }
            }
        }
        Err(e) => {
            tracing::error!(
                client = %client_pubkey,
                error = %e,
                "Failed to start rmcp service for client"
            );
        }
    }
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

    if let Err(e) = transport
        .send_notification(client_pubkey, &error_response, Some(event_id))
        .await
    {
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
            client_channels: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        assert_eq!(state.select_worker(), Some(0));
        assert_eq!(state.select_worker(), Some(1));
        assert_eq!(state.select_worker(), Some(2));
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
            client_channels: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        state.assign_client("client_a", 0);
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
            client_channels: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        state.assign_client("client_a", 0);
        state.assign_client("client_b", 1);
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
            client_channels: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

        state.assign_client("client_a", 0);
        state.assign_client("client_b", 1);

        assert_eq!(state.client_affinity.get("client_a"), Some(&0));
        assert_eq!(state.client_affinity.get("client_b"), Some(&1));

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
            client_channels: HashMap::new(),
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
            client_channels: HashMap::new(),
            next_worker: 0,
            wait_queue: VecDeque::new(),
            config,
        };

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

        state.drain_wait_queue();
        assert_eq!(state.wait_queue.len(), 0);
        assert!(state.client_affinity.contains_key("queued_client"));

        assert!(rx.try_recv().is_ok());
    }
}
