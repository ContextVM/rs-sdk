#![warn(missing_docs)]
//! # ContextVM SDK for Rust
//!
//! A complete Rust implementation of the [ContextVM protocol](https://contextvm.org),
//! enabling MCP (Model Context Protocol) servers to expose their capabilities through
//! the Nostr network.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  Gateway / Proxy  (high-level)          │
//! ├─────────────────────────────────────────┤
//! │  Transport  (client / server)           │
//! ├─────────────────────────────────────────┤
//! │  Core  (types, serializers, validation) │
//! │  Relay  │  Signer  │  Encryption        │
//! └─────────────────────────────────────────┘
//! ```
//!
//! ## Quick Start
//!
//! ### As a Gateway (expose local MCP server via Nostr)
//!
//! ```rust,no_run
//! use contextvm_sdk::gateway::{NostrMCPGateway, GatewayConfig};
//! use contextvm_sdk::transport::server::NostrServerTransportConfig;
//! use contextvm_sdk::core::types::ServerInfo;
//! use contextvm_sdk::signer;
//! ```
//!
//! ### As a Proxy (connect to remote MCP server via Nostr)
//!
//! ```rust,no_run
//! use contextvm_sdk::proxy::{NostrMCPProxy, ProxyConfig};
//! use contextvm_sdk::transport::client::NostrClientTransportConfig;
//! use contextvm_sdk::signer;
//! ```

/// Core types, constants, serializers, and validation
pub mod core;
/// Server and capability discovery on the Nostr network
pub mod discovery;
/// NIP-44 encryption and NIP-59 gift wrapping
pub mod encryption;
/// Gateway bridging a local MCP server to Nostr
pub mod gateway;
/// Proxy connecting to a remote MCP server via Nostr
pub mod proxy;
/// Nostr relay pool management
pub mod relay;
/// Nostr signer utilities and key management
pub mod signer;
/// Client and server MCP-over-Nostr transports
pub mod transport;

/// rmcp Worker integration for ContextVM transports
#[cfg(feature = "rmcp")]
pub mod rmcp_transport;
// ── Core types and error handling ────────────────────────────────────
pub use core::error::{Error, Result};
pub use core::types::{
    CapabilityExclusion, ClientSession, EncryptionMode, GiftWrapMode, JsonRpcError,
    JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    ProfileMetadata, ServerInfo,
};

// ── Discovery ────────────────────────────────────────────────────────
pub use discovery::ServerAnnouncement;

// ── Relay pool ───────────────────────────────────────────────────────
#[cfg(any(test, feature = "test-utils"))]
pub use relay::mock::MockRelayPool;
pub use relay::{RelayPool, RelayPoolTrait};

// ── Transport (client) ──────────────────────────────────────────────
pub use transport::client::{
    ClientCorrelationStore, NostrClientTransport, NostrClientTransportConfig,
};

// ── Transport (discovery tags) ──────────────────────────────────────
pub use transport::discovery_tags::{DiscoveredPeerCapabilities, PeerCapabilities};

// ── Transport (server) ──────────────────────────────────────────────
pub use transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig, RouteEntry,
    ServerEventRouteStore, SessionSnapshot, SessionStore,
};

// ── rmcp re-export ──────────────────────────────────────────────────
#[cfg(feature = "rmcp")]
pub use rmcp;

// ── CEP-22 progress-aware request helpers ───────────────────────────
#[cfg(feature = "rmcp")]
pub use rmcp_transport::progress::{
    progress_aware_options, PeerRequestOptionsExt, DEFAULT_OVERSIZED_IDLE_TIMEOUT,
    DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT,
};

// ── CEP-41 open-stream consumer API ─────────────────────────────────
#[cfg(feature = "rmcp")]
pub use rmcp_transport::open_stream::{call_tool_stream, ToolStreamCall};
#[cfg(feature = "rmcp")]
pub use transport::client::ClientOpenStreamHandle;
