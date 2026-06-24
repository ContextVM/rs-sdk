//! Transport layer for ContextVM — MCP over Nostr.
//!
//! Provides client and server transports that implement the MCP Transport pattern
//! using Nostr events for communication.

pub mod base;
pub mod client;
pub mod discovery_tags;
pub mod open_stream;
pub mod oversized_transfer;
pub mod server;

pub use client::{ClientCorrelationStore, NostrClientTransport, NostrClientTransportConfig};
pub use discovery_tags::*;
pub use open_stream::{
    OpenStreamConfig, OpenStreamError, OpenStreamFrame, OpenStreamRegistry,
    OpenStreamRegistryPolicy, OpenStreamSession, OpenStreamWriter,
};
pub use server::{NostrServerTransport, NostrServerTransportConfig, ServerEventRouteStore};
