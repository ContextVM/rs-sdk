//! CEP-22 `cvm` frame types and their `notifications/progress` envelope.
//!
//! A frame is the ContextVM `cvm` extension object embedded in the `params` of
//! an MCP `notifications/progress` notification. The outer `params` also carry
//! the `progressToken` and a strictly-monotonic `progress` value (the canonical
//! reassembly index). See `frame` shapes in
//! `sdk/src/transport/oversized-transfer/types.ts`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::types::JsonRpcNotification;

use super::constants::{NOTIFICATIONS_PROGRESS_METHOD, OVERSIZED_TRANSFER_TYPE};

/// Completion mode of a `start` frame. CEP-22 v1 mandates `render`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompletionMode {
    /// The only completion mode defined in v1: the receiver renders the
    /// reassembled payload as a single MCP message.
    #[default]
    Render,
}

/// A CEP-22 oversized-transfer frame (the `cvm` object).
///
/// Internally tagged on `frameType`. The constant `cvm.type`
/// (`"oversized-transfer"`) is handled separately by
/// [`to_cvm_value`](Self::to_cvm_value) / [`from_cvm_value`](Self::from_cvm_value)
/// rather than encoded as an enum field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frameType", rename_all = "camelCase")]
pub enum OversizedFrame {
    /// Opens a transfer: declares the digest, total byte length, and chunk count.
    #[serde(rename_all = "camelCase")]
    Start {
        /// Completion mode (MUST be [`CompletionMode::Render`] in v1).
        completion_mode: CompletionMode,
        /// `"sha256:"` + lowercase hex of the SHA-256 of the serialized payload.
        digest: String,
        /// Total UTF-8 byte length of the serialized JSON-RPC payload.
        total_bytes: u64,
        /// Number of `chunk` frames that follow.
        total_chunks: u64,
    },
    /// Receiver handshake acknowledgement; lets the sender begin sending chunks.
    Accept,
    /// One ordered fragment of the serialized JSON-RPC string.
    Chunk {
        /// A UTF-8 substring fragment; concatenating chunks in `progress` order
        /// reproduces the exact serialized payload bytes.
        data: String,
    },
    /// Closes a transfer; the receiver validates and materializes the payload.
    End,
    /// Terminates a transfer early (terminal).
    Abort {
        /// Optional advisory reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl OversizedFrame {
    /// Returns the `frameType` discriminator string for this frame.
    pub fn frame_type(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Accept => "accept",
            Self::Chunk { .. } => "chunk",
            Self::End => "end",
            Self::Abort { .. } => "abort",
        }
    }

    /// Serialize this frame into a `cvm` object [`Value`], injecting the constant
    /// `type` discriminator (`"oversized-transfer"`).
    pub fn to_cvm_value(&self) -> Result<Value, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        if let Value::Object(map) = &mut value {
            map.insert(
                "type".to_string(),
                Value::String(OVERSIZED_TRANSFER_TYPE.to_string()),
            );
        }
        Ok(value)
    }

    /// Parse a `cvm` object [`Value`] into a typed frame, verifying the `type`
    /// discriminator. Returns `None` when `value` is not an oversized-transfer
    /// frame or fails to parse.
    pub fn from_cvm_value(value: &Value) -> Option<Self> {
        if value.get("type").and_then(Value::as_str) != Some(OVERSIZED_TRANSFER_TYPE) {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }

    /// Returns `true` when `value` structurally looks like an oversized-transfer
    /// frame (`type == "oversized-transfer"` and a string `frameType`).
    ///
    /// Mirrors the TS `isOversizedTransferFrame` structural narrowing.
    pub fn is_frame_value(value: &Value) -> bool {
        value.get("type").and_then(Value::as_str) == Some(OVERSIZED_TRANSFER_TYPE)
            && value.get("frameType").and_then(Value::as_str).is_some()
    }

    /// Wrap this frame in a `notifications/progress` [`JsonRpcNotification`].
    ///
    /// Builds the outer `params` with `progressToken`, `progress`, an optional
    /// human-readable `message` (non-normative UX), and the `cvm` frame. The
    /// token is always emitted as a JSON **string**, even when the originating
    /// request carried a numeric one (matching the TS SDK's
    /// `String(progressToken)`); see [`progress_token_string`].
    pub fn into_progress_notification(
        &self,
        progress_token: &str,
        progress: u64,
        message: Option<&str>,
    ) -> Result<JsonRpcNotification, serde_json::Error> {
        let mut params = Map::new();
        params.insert(
            "progressToken".to_string(),
            Value::String(progress_token.to_string()),
        );
        params.insert("progress".to_string(), Value::Number(progress.into()));
        if let Some(message) = message {
            params.insert("message".to_string(), Value::String(message.to_string()));
        }
        params.insert("cvm".to_string(), self.to_cvm_value()?);

        Ok(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: NOTIFICATIONS_PROGRESS_METHOD.to_string(),
            params: Some(Value::Object(params)),
        })
    }
}

/// Coerce a `progressToken` JSON value to its canonical string form.
///
/// MCP progress tokens may be JSON strings **or numbers** (rmcp stamps a
/// numeric token into every outgoing request), so token extraction must accept
/// both; all transport-internal keying (correlation routes, accept waiters,
/// reassembly state, frame addressing) uses the stringified form. The wire
/// format is unchanged — frames always carry a string token
/// ([`OversizedFrame::into_progress_notification`]), exactly like the TS SDK's
/// `String(progressToken)`. Returns `None` for any other JSON type.
pub fn progress_token_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_cvm_roundtrip(frame: OversizedFrame) {
        let value = frame.to_cvm_value().unwrap();
        assert_eq!(value["type"], json!("oversized-transfer"));
        assert_eq!(value["frameType"], json!(frame.frame_type()));
        assert_eq!(OversizedFrame::from_cvm_value(&value), Some(frame));
    }

    #[test]
    fn start_frame_serializes_with_camel_case_fields() {
        let frame = OversizedFrame::Start {
            completion_mode: CompletionMode::Render,
            digest: "sha256:abcd".to_string(),
            total_bytes: 100,
            total_chunks: 3,
        };
        let value = frame.to_cvm_value().unwrap();
        assert_eq!(value["type"], json!("oversized-transfer"));
        assert_eq!(value["frameType"], json!("start"));
        assert_eq!(value["completionMode"], json!("render"));
        assert_eq!(value["digest"], json!("sha256:abcd"));
        assert_eq!(value["totalBytes"], json!(100));
        assert_eq!(value["totalChunks"], json!(3));
        assert_cvm_roundtrip(frame);
    }

    #[test]
    fn all_frame_variants_roundtrip() {
        assert_cvm_roundtrip(OversizedFrame::Start {
            completion_mode: CompletionMode::Render,
            digest: "sha256:abcd".to_string(),
            total_bytes: 8,
            total_chunks: 2,
        });
        assert_cvm_roundtrip(OversizedFrame::Accept);
        assert_cvm_roundtrip(OversizedFrame::Chunk {
            data: "payload".to_string(),
        });
        assert_cvm_roundtrip(OversizedFrame::End);
        assert_cvm_roundtrip(OversizedFrame::Abort {
            reason: Some("boom".to_string()),
        });
        assert_cvm_roundtrip(OversizedFrame::Abort { reason: None });
    }

    #[test]
    fn abort_reason_omitted_when_none() {
        let value = OversizedFrame::Abort { reason: None }
            .to_cvm_value()
            .unwrap();
        assert!(!value.as_object().unwrap().contains_key("reason"));
    }

    #[test]
    fn from_cvm_value_rejects_wrong_type() {
        let value = json!({ "type": "something-else", "frameType": "end" });
        assert_eq!(OversizedFrame::from_cvm_value(&value), None);
        assert!(!OversizedFrame::is_frame_value(&value));
    }

    #[test]
    fn from_cvm_value_rejects_unknown_frame_type() {
        let value = json!({ "type": "oversized-transfer", "frameType": "bogus" });
        assert_eq!(OversizedFrame::from_cvm_value(&value), None);
    }

    #[test]
    fn is_frame_value_requires_type_and_frame_type() {
        assert!(OversizedFrame::is_frame_value(
            &json!({ "type": "oversized-transfer", "frameType": "chunk", "data": "x" })
        ));
        assert!(!OversizedFrame::is_frame_value(
            &json!({ "type": "oversized-transfer" })
        ));
        assert!(!OversizedFrame::is_frame_value(
            &json!({ "frameType": "chunk" })
        ));
        assert!(!OversizedFrame::is_frame_value(&json!("not an object")));
    }

    #[test]
    fn into_progress_notification_builds_progress_envelope() {
        let notification = OversizedFrame::Chunk {
            data: "frag".to_string(),
        }
        .into_progress_notification("tok-1", 7, Some("hi"))
        .unwrap();

        assert_eq!(notification.method, "notifications/progress");
        let params = notification.params.as_ref().unwrap();
        assert_eq!(params["progressToken"], json!("tok-1"));
        assert_eq!(params["progress"], json!(7));
        assert_eq!(params["message"], json!("hi"));
        assert_eq!(params["cvm"]["frameType"], json!("chunk"));
        assert_eq!(params["cvm"]["data"], json!("frag"));
    }

    #[test]
    fn into_progress_notification_omits_absent_message() {
        let notification = OversizedFrame::End
            .into_progress_notification("tok-1", 9, None)
            .unwrap();
        let params = notification.params.as_ref().unwrap();
        assert!(!params.as_object().unwrap().contains_key("message"));
    }

    #[test]
    fn progress_token_string_accepts_string_and_number() {
        assert_eq!(
            progress_token_string(&json!("tok-1")),
            Some("tok-1".to_string())
        );
        assert_eq!(progress_token_string(&json!(7)), Some("7".to_string()));
        assert_eq!(progress_token_string(&json!(0)), Some("0".to_string()));
        assert_eq!(progress_token_string(&json!(-3)), Some("-3".to_string()));
        assert_eq!(progress_token_string(&json!(7.5)), Some("7.5".to_string()));
    }

    #[test]
    fn progress_token_string_rejects_other_types() {
        assert_eq!(progress_token_string(&json!(null)), None);
        assert_eq!(progress_token_string(&json!(true)), None);
        assert_eq!(progress_token_string(&json!({ "t": 1 })), None);
        assert_eq!(progress_token_string(&json!([1])), None);
    }
}
