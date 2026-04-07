//! Message validation utilities.

use crate::core::constants::MAX_MESSAGE_SIZE;
use crate::core::types::JsonRpcMessage;

/// Validate that a message's content doesn't exceed the maximum size (1MB).
pub fn validate_message_size(content: &str) -> bool {
    content.len() <= MAX_MESSAGE_SIZE
}

/// Validate size and structure, then parse into a [`JsonRpcMessage`].
pub fn validate_and_parse(content: &str) -> Option<JsonRpcMessage> {
    if !validate_message_size(content) {
        tracing::warn!("Message size validation failed: {} bytes", content.len());
        return None;
    }

    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    validate_message(&value)
}

/// Validate that a JSON value is a well-formed JSON-RPC 2.0 message.
///
/// Checks:
/// - Has `jsonrpc: "2.0"`
/// - Is one of: request (id + method), response (id + result), error (id + error), notification (method, no id)
pub fn validate_message(value: &serde_json::Value) -> Option<JsonRpcMessage> {
    // Must have jsonrpc field
    let jsonrpc = value.get("jsonrpc")?.as_str()?;
    if jsonrpc != "2.0" {
        return None;
    }

    serde_json::from_value(value.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_valid_request() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let parsed = validate_message(&msg).unwrap();
        assert!(parsed.is_request());
    }

    #[test]
    fn test_valid_notification() {
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let parsed = validate_message(&msg).unwrap();
        assert!(parsed.is_notification());
    }

    #[test]
    fn test_valid_response() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        let parsed = validate_message(&msg).unwrap();
        assert!(parsed.is_response());
    }

    #[test]
    fn test_invalid_version() {
        let msg = json!({"jsonrpc": "1.0", "id": 1, "method": "test"});
        assert!(validate_message(&msg).is_none());
    }

    #[test]
    fn test_size_validation() {
        assert!(validate_message_size("hello"));
        let big = "x".repeat(MAX_MESSAGE_SIZE + 1);
        assert!(!validate_message_size(&big));
    }

    #[test]
    fn test_validate_and_parse_valid_request() {
        let content = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let msg = validate_and_parse(content).unwrap();
        assert!(msg.is_request());
        assert_eq!(msg.method(), Some("tools/list"));
    }

    #[test]
    fn test_validate_and_parse_rejects_oversized() {
        let padding = "x".repeat(MAX_MESSAGE_SIZE);
        let content = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{}"}}"#, padding);
        assert!(validate_and_parse(&content).is_none());
    }

    #[test]
    fn test_validate_and_parse_rejects_invalid_version() {
        let content = r#"{"jsonrpc":"1.0","id":1,"method":"test"}"#;
        assert!(validate_and_parse(content).is_none());
    }

    #[test]
    fn test_validate_and_parse_rejects_invalid_json() {
        assert!(validate_and_parse("not json").is_none());
    }
}
