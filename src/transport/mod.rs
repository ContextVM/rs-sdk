//! Transport layer for ContextVM — MCP over Nostr.
//!
//! Provides client and server transports that implement the MCP Transport pattern
//! using Nostr events for communication.

pub mod base;
pub mod client;
mod dedup;
pub mod server;

pub use client::{NostrClientTransport, NostrClientTransportConfig};
pub use server::{NostrServerTransport, NostrServerTransportConfig};
