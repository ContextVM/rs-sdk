//! rmcp integration for ContextVM Nostr transports.
//!
//! This module contains the conversion helpers and worker bridge that let raw
//! ContextVM transports plug directly into rmcp service APIs.

pub mod convert;
pub mod open_stream;
pub mod progress;
pub mod transport;
pub mod worker;

#[cfg(test)]
mod pipeline_tests;

pub use convert::{
    internal_to_rmcp_client_rx, internal_to_rmcp_server_rx, rmcp_client_tx_to_internal,
    rmcp_server_tx_to_internal,
};
pub use open_stream::{call_tool_stream, ToolStreamCall};
pub use progress::{
    progress_aware_options, PeerRequestOptionsExt, DEFAULT_OVERSIZED_IDLE_TIMEOUT,
    DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT,
};
pub use worker::{NostrClientWorker, NostrServerWorker};
