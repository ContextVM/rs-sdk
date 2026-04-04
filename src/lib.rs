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

pub mod core;
pub mod discovery;
pub mod encryption;
pub mod gateway;
pub mod proxy;
pub mod relay;
pub mod signer;
pub mod transport;
pub mod util;

#[cfg(feature = "rmcp")]
pub mod rmcp_transport;

// Re-export commonly used types
pub use core::error::{Error, Result};
pub use core::types::{
    CapabilityExclusion, ClientSession, EncryptionMode, JsonRpcError, JsonRpcErrorResponse,
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ServerInfo,
};
pub use discovery::ServerAnnouncement;
pub use relay::RelayPool;
pub use transport::client::{NostrClientTransport, NostrClientTransportConfig};
pub use transport::server::{IncomingRequest, NostrServerTransport, NostrServerTransportConfig};

#[cfg(feature = "rmcp")]
pub use rmcp;
