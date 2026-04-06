//! Core types for the ContextVM protocol

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

// ── Encryption mode ─────────────────────────────────────────────────

/// Encryption mode for transport communication.
///
/// Controls whether MCP messages are sent as plaintext kind 25910 events
/// or wrapped in NIP-59 gift wraps (kind 1059) for end-to-end encryption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncryptionMode {
    /// Encrypt responses only when the incoming request was encrypted (mirror mode).
    #[default]
    Optional,
    /// Enforce encryption for all messages; reject plaintext.
    Required,
    /// Disable encryption entirely; all messages are plaintext kind 25910.
    Disabled,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_encryption_mode_serde_roundtrip_optional() {
        let mode = EncryptionMode::Optional;
        let s = serde_json::to_string(&mode).unwrap();
        assert_eq!(s, "\"optional\"");
        let parsed: EncryptionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, mode);
    }

    #[test]
    fn test_encryption_mode_serde_roundtrip_required() {
        let mode = EncryptionMode::Required;
        let s = serde_json::to_string(&mode).unwrap();
        assert_eq!(s, "\"required\"");
        let parsed: EncryptionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, mode);
    }

    #[test]
    fn test_encryption_mode_serde_roundtrip_disabled() {
        let mode = EncryptionMode::Disabled;
        let s = serde_json::to_string(&mode).unwrap();
        assert_eq!(s, "\"disabled\"");
        let parsed: EncryptionMode = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, mode);
    }

    fn assert_json_rpc_roundtrip(msg: &JsonRpcMessage) {
        let wire = serde_json::to_string(msg).unwrap();
        let parsed: JsonRpcMessage = serde_json::from_str(&wire).unwrap();
        let before = serde_json::to_value(msg).unwrap();
        let after = serde_json::to_value(&parsed).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn test_json_rpc_message_serde_roundtrip_request() {
        let msg = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(42),
            method: "tools/list".to_string(),
            params: Some(json!({ "cursor": null })),
        });
        assert_json_rpc_roundtrip(&msg);
    }

    #[test]
    fn test_json_rpc_message_serde_roundtrip_request_without_params() {
        let msg = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!("req-id"),
            method: "ping".to_string(),
            params: None,
        });
        assert_json_rpc_roundtrip(&msg);
    }

    #[test]
    fn test_json_rpc_message_serde_roundtrip_response() {
        let msg = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            result: json!({ "tools": [] }),
        });
        assert_json_rpc_roundtrip(&msg);
    }

    #[test]
    fn test_json_rpc_message_serde_roundtrip_error_response() {
        let msg = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(99),
            error: JsonRpcError {
                code: -32600,
                message: "Invalid Request".to_string(),
                data: Some(json!({ "hint": "fix it" })),
            },
        });
        assert_json_rpc_roundtrip(&msg);
    }

    #[test]
    fn test_json_rpc_message_serde_roundtrip_notification() {
        let msg = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        });
        assert_json_rpc_roundtrip(&msg);
    }

    #[test]
    fn test_json_rpc_message_type_predicates() {
        let req = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            method: "m".to_string(),
            params: None,
        });
        let res = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            result: json!(null),
        });
        let err = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(1),
            error: JsonRpcError {
                code: -1,
                message: "e".to_string(),
                data: None,
            },
        });
        let notif = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "n".to_string(),
            params: None,
        });

        assert!(req.is_request());
        assert!(res.is_response());
        assert!(err.is_error());
        assert!(notif.is_notification());
    }

    #[test]
    fn test_json_rpc_error_data_none_omitted() {
        let err = JsonRpcError {
            code: -32600,
            message: "bad".to_string(),
            data: None,
        };
        let json_str = serde_json::to_string(&err).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = value.as_object().expect("error object");
        assert!(
            !obj.contains_key("data"),
            "expected data omitted when None, got: {json_str}"
        );
    }

    #[test]
    fn test_json_rpc_message_method() {
        let req = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(0),
            method: "tools/call".to_string(),
            params: None,
        });
        let res = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(0),
            result: json!(null),
        });
        let err = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(0),
            error: JsonRpcError {
                code: 0,
                message: "e".to_string(),
                data: None,
            },
        });
        let notif = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: None,
        });

        assert_eq!(req.method(), Some("tools/call"));
        assert_eq!(res.method(), None);
        assert_eq!(err.method(), None);
        assert_eq!(notif.method(), Some("notifications/progress"));
    }

    #[test]
    fn test_json_rpc_message_id() {
        let req = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!("abc"),
            method: "m".to_string(),
            params: None,
        });
        let res = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(7),
            result: json!(null),
        });
        let err = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: json!([1, 2]),
            error: JsonRpcError {
                code: 0,
                message: "e".to_string(),
                data: None,
            },
        });
        let notif = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "n".to_string(),
            params: None,
        });

        assert_eq!(req.id(), Some(&json!("abc")));
        assert_eq!(res.id(), Some(&json!(7)));
        assert_eq!(err.id(), Some(&json!([1, 2])));
        assert_eq!(notif.id(), None);
    }

    #[test]
    fn test_server_info_serde_all_fields_present() {
        let info = ServerInfo {
            name: Some("Test Server".to_string()),
            version: Some("1.0.0".to_string()),
            picture: Some("https://example.com/p.png".to_string()),
            website: Some("https://example.com".to_string()),
            about: Some("About text".to_string()),
        };
        let json_str = serde_json::to_string(&info).unwrap();
        let parsed: ServerInfo = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.name, info.name);
        assert_eq!(parsed.version, info.version);
        assert_eq!(parsed.picture, info.picture);
        assert_eq!(parsed.website, info.website);
        assert_eq!(parsed.about, info.about);
    }

    #[test]
    fn test_server_info_serde_optional_fields_omitted() {
        let info = ServerInfo {
            name: None,
            version: None,
            picture: None,
            website: None,
            about: None,
        };
        let json_str = serde_json::to_string(&info).unwrap();
        assert_eq!(json_str, "{}");
    }

    #[test]
    fn test_client_session_new_initial_state_encrypted() {
        let session = ClientSession::new(true);
        assert!(!session.is_initialized);
        assert!(session.is_encrypted);
        assert!(session.pending_requests.is_empty());
        assert!(session.event_to_progress_token.is_empty());
    }

    #[test]
    fn test_client_session_new_initial_state_plaintext() {
        let session = ClientSession::new(false);
        assert!(!session.is_initialized);
        assert!(!session.is_encrypted);
        assert!(session.pending_requests.is_empty());
        assert!(session.event_to_progress_token.is_empty());
    }

    #[test]
    fn test_client_session_update_activity() {
        let mut session = ClientSession::new(false);
        let first = session.last_activity;
        thread::sleep(Duration::from_millis(10));
        session.update_activity();
        assert!(session.last_activity > first);
    }
}
