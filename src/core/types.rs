//! Core types for the ContextVM protocol

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

// ── Encryption mode ─────────────────────────────────────────────────

/// Encryption mode for transport communication.
///
/// Controls whether MCP messages are sent as plaintext kind 25910 events
/// or wrapped in NIP-59 gift wraps (kind 1059) for end-to-end encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncryptionMode {
    /// Encrypt responses only when the incoming request was encrypted (mirror mode).
    Optional,
    /// Enforce encryption for all messages; reject plaintext.
    Required,
    /// Disable encryption entirely; all messages are plaintext kind 25910.
    Disabled,
}

impl Default for EncryptionMode {
    fn default() -> Self {
        Self::Optional
    }
}

// ── Server info ─────────────────────────────────────────────────────

/// Server information for announcements (kind 11316).
///
/// Published as the content of a replaceable Nostr event so that clients
/// can discover the server's identity and metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Human-readable server name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Server software version string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// URL to the server's avatar or logo image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// Server's website URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// Short description of the server's purpose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

// ── Client session ──────────────────────────────────────────────────

/// Client session state tracked by the server transport.
#[derive(Debug)]
pub struct ClientSession {
    /// Whether the client has completed MCP initialization.
    pub is_initialized: bool,
    /// Whether the client's messages were encrypted.
    pub is_encrypted: bool,
    /// Last activity timestamp.
    pub last_activity: Instant,
    /// Pending requests: event_id → original request ID.
    pub pending_requests: HashMap<String, serde_json::Value>,
    /// Progress token tracking: event_id → progress token string.
    pub event_to_progress_token: HashMap<String, String>,
}

impl ClientSession {
    /// Create a new client session, recording whether the initial message was encrypted.
    pub fn new(is_encrypted: bool) -> Self {
        Self {
            is_initialized: false,
            is_encrypted,
            last_activity: Instant::now(),
            pending_requests: HashMap::new(),
            event_to_progress_token: HashMap::new(),
        }
    }

    /// Touch the session, updating [`last_activity`](Self::last_activity) to now.
    pub fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }
}

// ── JSON-RPC types ──────────────────────────────────────────────────
//
// MCP uses JSON-RPC 2.0. We define our own types here since there's
// no official Rust MCP SDK. These are wire-compatible with the MCP spec.

/// A JSON-RPC 2.0 message (request, response, notification, or error).
///
/// This is the primary message type exchanged between MCP clients and servers.
/// Deserialized using `#[serde(untagged)]` to match any of the four variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    /// A request expecting a response (has `id` and `method`).
    Request(JsonRpcRequest),
    /// A successful response (has `id` and `result`).
    Response(JsonRpcResponse),
    /// An error response (has `id` and `error`).
    ErrorResponse(JsonRpcErrorResponse),
    /// A notification (has `method`, no `id`, no response expected).
    Notification(JsonRpcNotification),
}

/// A JSON-RPC 2.0 request.
///
/// Contains a method name and an optional params object. The `id` field
/// is used to correlate the response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Request identifier for response correlation.
    pub id: serde_json::Value,
    /// The RPC method name (e.g., `"tools/list"`, `"tools/call"`).
    pub method: String,
    /// Optional method parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 successful response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// The request ID this response corresponds to.
    pub id: serde_json::Value,
    /// The result payload.
    pub result: serde_json::Value,
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// The request ID this error corresponds to.
    pub id: serde_json::Value,
    /// The error object describing what went wrong.
    pub error: JsonRpcError,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code (e.g., `-32600` for invalid request).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional additional error data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 notification (no `id`, no response expected).
///
/// Used for one-way messages like `notifications/initialized`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// The notification method name.
    pub method: String,
    /// Optional notification parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

// ── Helpers ─────────────────────────────────────────────────────────

impl JsonRpcMessage {
    /// Check if this is a request (has id + method).
    pub fn is_request(&self) -> bool {
        matches!(self, Self::Request(_))
    }

    /// Check if this is a response (has id + result).
    pub fn is_response(&self) -> bool {
        matches!(self, Self::Response(_))
    }

    /// Check if this is an error response (has id + error).
    pub fn is_error(&self) -> bool {
        matches!(self, Self::ErrorResponse(_))
    }

    /// Check if this is a notification (has method, no id).
    pub fn is_notification(&self) -> bool {
        matches!(self, Self::Notification(_))
    }

    /// Get the method name if this is a request or notification.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request(r) => Some(&r.method),
            Self::Notification(n) => Some(&n.method),
            _ => None,
        }
    }

    /// Get the request/response id if present.
    pub fn id(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Request(r) => Some(&r.id),
            Self::Response(r) => Some(&r.id),
            Self::ErrorResponse(r) => Some(&r.id),
            Self::Notification(_) => None,
        }
    }
}

// ── Capability exclusion ────────────────────────────────────────────

/// A capability exclusion pattern that bypasses pubkey whitelisting.
#[derive(Debug, Clone)]
pub struct CapabilityExclusion {
    /// The JSON-RPC method to exclude (e.g., "tools/call", "tools/list").
    pub method: String,
    /// Optional capability name for method-specific exclusions (e.g., "get_weather").
    pub name: Option<String>,
}
