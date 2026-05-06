//! rmcp integration for ContextVM Nostr transports.
//!
//! This module contains the conversion helpers and worker bridge that let raw
//! ContextVM transports plug directly into rmcp service APIs.

pub mod convert;
pub mod transport;
pub mod worker;

#[cfg(test)]
mod pipeline_tests;

pub use convert::{
    internal_to_rmcp_client_rx, internal_to_rmcp_server_rx, rmcp_client_tx_to_internal,
    rmcp_server_tx_to_internal,
};
pub use worker::{NostrClientWorker, NostrServerWorker};
