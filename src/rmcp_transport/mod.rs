//! RMCP integration scaffolding.
//!
//! This module bridges the existing Nostr transport implementation with rmcp services.
//!
//! # Worker Pool (multi-client)
//!
//! The [`pool`] module provides a `WorkerPool` that manages multiple workers,
//! distributing incoming client peers with round-robin assignment and affinity.
//! This is the recommended approach for production servers that must handle
//! multiple concurrent clients.
//!
//! # Single Worker (deprecated)
//!
//! The [`NostrServerWorker`] provides a single-peer worker for simple use cases.
//! It is deprecated in favour of the worker pool.

pub mod convert;
pub mod pool;
pub mod pool_transport;
pub mod worker;

#[cfg(test)]
mod pipeline_tests;

pub use convert::{
    internal_to_rmcp_client_rx, internal_to_rmcp_server_rx, rmcp_client_tx_to_internal,
    rmcp_server_tx_to_internal,
};
pub use pool::{start_worker_pool, WorkerPoolConfig, WorkerPoolHandle, WorkerPoolStats};
#[allow(deprecated)]
pub use worker::{NostrClientWorker, NostrServerWorker};
