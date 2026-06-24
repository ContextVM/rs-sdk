//! CEP-41 `cvm` frame types and their `notifications/progress` envelope.
//!
//! A frame is the ContextVM `cvm` extension object embedded in the `params` of
//! an MCP `notifications/progress` notification. The outer `params` also carry
//! the `progressToken` (the stream id) and a strictly-monotonic `progress`
//! value that orders *all* frames in one direction. See the frame shapes in
//! `sdk/src/transport/open-stream/types.ts`.
//!
//! Unlike CEP-22, an open-stream `chunk` also carries a `chunkIndex` (contiguous
//! from 0) that tracks payload completeness independently of the outer
//! `progress` counter.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::types::JsonRpcNotification;
use crate::transport::oversized_transfer::NOTIFICATIONS_PROGRESS_METHOD;

use super::constants::OPEN_STREAM_TYPE;

/// A CEP-41 open-stream frame (the `cvm` object).
///
/// Internally tagged on `frameType`. The constant `cvm.type` (`"open-stream"`)
/// is handled separately by [`to_cvm_value`](Self::to_cvm_value) /
/// [`from_cvm_value`](Self::from_cvm_value) rather than encoded as an enum
/// field, exactly mirroring CEP-22's `oversized_transfer/frame.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frameType", rename_all = "camelCase")]
pub enum OpenStreamFrame {
    /// Opens a stream. Carries only optional application-defined advisory
    /// metadata; receivers MUST NOT depend on it for stream correctness.
    #[serde(rename_all = "camelCase")]
    Start {
        /// Optional advisory content type (writer-settable, receiver-ignored).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    /// Reader handshake acknowledgement, sent only in reply to an inbound `start`.
    Accept,
    /// One ordered payload fragment of the stream.
    #[serde(rename_all = "camelCase")]
    Chunk {
        /// Contiguous-from-0 chunk index used for completeness/ordering.
        chunk_index: u64,
        /// The chunk payload (an arbitrary UTF-8 string).
        data: String,
    },
    /// Keepalive probe; the peer must echo the `nonce` in a `pong`.
    Ping {
        /// Opaque probe nonce echoed back by the peer.
        nonce: String,
    },
    /// Keepalive response echoing a prior `ping` nonce.
    Pong {
        /// The nonce of the `ping` being acknowledged.
        nonce: String,
    },
    /// Closes the stream gracefully (terminal). Declares the final chunk index
    /// when any chunks were emitted, so the reader can verify completeness.
    #[serde(rename_all = "camelCase")]
    Close {
        /// The highest `chunkIndex` emitted, or `None` for a zero-chunk stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_chunk_index: Option<u64>,
    },
    /// Terminates the stream early (terminal).
    Abort {
        /// Optional advisory reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl OpenStreamFrame {
    /// Returns the `frameType` discriminator string for this frame.
    pub fn frame_type(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Accept => "accept",
            Self::Chunk { .. } => "chunk",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::Close { .. } => "close",
            Self::Abort { .. } => "abort",
        }
    }

    /// Serialize this frame into a `cvm` object [`Value`], injecting the constant
    /// `type` discriminator (`"open-stream"`).
    pub fn to_cvm_value(&self) -> Result<Value, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        if let Value::Object(map) = &mut value {
            map.insert(
                "type".to_string(),
                Value::String(OPEN_STREAM_TYPE.to_string()),
            );
        }
        Ok(value)
    }

    /// Parse a `cvm` object [`Value`] into a typed frame, verifying the `type`
    /// discriminator. Returns `None` when `value` is not an open-stream frame or
    /// fails to parse.
    pub fn from_cvm_value(value: &Value) -> Option<Self> {
        if value.get("type").and_then(Value::as_str) != Some(OPEN_STREAM_TYPE) {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }

    /// Returns `true` when `value` structurally looks like an open-stream frame
    /// (`type == "open-stream"` and a string `frameType`).
    ///
    /// Mirrors the TS `isOpenStreamFrame` structural narrowing.
    pub fn is_frame_value(value: &Value) -> bool {
        value.get("type").and_then(Value::as_str) == Some(OPEN_STREAM_TYPE)
            && value.get("frameType").and_then(Value::as_str).is_some()
    }

    /// Wrap this frame in a `notifications/progress` [`JsonRpcNotification`].
    ///
    /// Builds the outer `params` with `progressToken`, `progress`, an optional
    /// human-readable `message` (non-normative UX), and the `cvm` frame. The
    /// token is always emitted as a JSON **string**, even when the originating
    /// request carried a numeric one (matching the TS SDK's
    /// `String(progressToken)`); see
    /// [`progress_token_string`](crate::transport::oversized_transfer::progress_token_string).
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

/// Extract a typed [`OpenStreamFrame`] from a `notifications/progress` payload's
/// `params.cvm`, returning `None` for any non-open-stream notification.
pub fn open_stream_frame_from_notification(
    notification: &JsonRpcNotification,
) -> Option<OpenStreamFrame> {
    notification
        .params
        .as_ref()
        .and_then(|params| params.get("cvm"))
        .and_then(OpenStreamFrame::from_cvm_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_cvm_roundtrip(frame: OpenStreamFrame) {
        let value = frame.to_cvm_value().unwrap();
        assert_eq!(value["type"], json!("open-stream"));
        assert_eq!(value["frameType"], json!(frame.frame_type()));
        assert_eq!(OpenStreamFrame::from_cvm_value(&value), Some(frame));
    }

    #[test]
    fn all_frame_variants_roundtrip() {
        assert_cvm_roundtrip(OpenStreamFrame::Start {
            content_type: Some("text/plain".to_string()),
        });
        assert_cvm_roundtrip(OpenStreamFrame::Start { content_type: None });
        assert_cvm_roundtrip(OpenStreamFrame::Accept);
        assert_cvm_roundtrip(OpenStreamFrame::Chunk {
            chunk_index: 7,
            data: "payload".to_string(),
        });
        assert_cvm_roundtrip(OpenStreamFrame::Ping {
            nonce: "tok:1".to_string(),
        });
        assert_cvm_roundtrip(OpenStreamFrame::Pong {
            nonce: "tok:1".to_string(),
        });
        assert_cvm_roundtrip(OpenStreamFrame::Close {
            last_chunk_index: Some(4),
        });
        assert_cvm_roundtrip(OpenStreamFrame::Close {
            last_chunk_index: None,
        });
        assert_cvm_roundtrip(OpenStreamFrame::Abort {
            reason: Some("boom".to_string()),
        });
        assert_cvm_roundtrip(OpenStreamFrame::Abort { reason: None });
    }

    #[test]
    fn chunk_frame_serializes_with_camel_case_fields() {
        let value = OpenStreamFrame::Chunk {
            chunk_index: 3,
            data: "abc".to_string(),
        }
        .to_cvm_value()
        .unwrap();
        assert_eq!(value["type"], json!("open-stream"));
        assert_eq!(value["frameType"], json!("chunk"));
        assert_eq!(value["chunkIndex"], json!(3));
        assert_eq!(value["data"], json!("abc"));
        // snake_case field name must not leak onto the wire.
        assert!(!value.as_object().unwrap().contains_key("chunk_index"));
    }

    #[test]
    fn start_content_type_serializes_camel_case_and_omits_when_none() {
        let with = OpenStreamFrame::Start {
            content_type: Some("application/json".to_string()),
        }
        .to_cvm_value()
        .unwrap();
        assert_eq!(with["contentType"], json!("application/json"));

        let without = OpenStreamFrame::Start { content_type: None }
            .to_cvm_value()
            .unwrap();
        assert!(!without.as_object().unwrap().contains_key("contentType"));
    }

    #[test]
    fn close_last_chunk_index_serializes_camel_case_and_omits_when_none() {
        let with = OpenStreamFrame::Close {
            last_chunk_index: Some(9),
        }
        .to_cvm_value()
        .unwrap();
        assert_eq!(with["lastChunkIndex"], json!(9));

        let without = OpenStreamFrame::Close {
            last_chunk_index: None,
        }
        .to_cvm_value()
        .unwrap();
        assert!(!without.as_object().unwrap().contains_key("lastChunkIndex"));
    }

    #[test]
    fn abort_reason_omitted_when_none() {
        let value = OpenStreamFrame::Abort { reason: None }
            .to_cvm_value()
            .unwrap();
        assert!(!value.as_object().unwrap().contains_key("reason"));
    }

    #[test]
    fn from_cvm_value_rejects_wrong_type() {
        let value = json!({ "type": "oversized-transfer", "frameType": "start" });
        assert_eq!(OpenStreamFrame::from_cvm_value(&value), None);
        assert!(!OpenStreamFrame::is_frame_value(&value));
    }

    #[test]
    fn from_cvm_value_rejects_unknown_frame_type() {
        let value = json!({ "type": "open-stream", "frameType": "bogus" });
        assert_eq!(OpenStreamFrame::from_cvm_value(&value), None);
    }

    #[test]
    fn is_frame_value_requires_type_and_frame_type() {
        assert!(OpenStreamFrame::is_frame_value(
            &json!({ "type": "open-stream", "frameType": "chunk", "chunkIndex": 0, "data": "x" })
        ));
        assert!(!OpenStreamFrame::is_frame_value(
            &json!({ "type": "open-stream" })
        ));
        assert!(!OpenStreamFrame::is_frame_value(
            &json!({ "frameType": "chunk" })
        ));
        assert!(!OpenStreamFrame::is_frame_value(&json!("not an object")));
    }

    #[test]
    fn into_progress_notification_builds_progress_envelope() {
        let notification = OpenStreamFrame::Chunk {
            chunk_index: 2,
            data: "frag".to_string(),
        }
        .into_progress_notification("tok-1", 7, Some("hi"))
        .unwrap();

        assert_eq!(notification.method, "notifications/progress");
        let params = notification.params.as_ref().unwrap();
        // progressToken is always a string on the wire.
        assert_eq!(params["progressToken"], json!("tok-1"));
        // progress is numeric.
        assert_eq!(params["progress"], json!(7));
        assert_eq!(params["message"], json!("hi"));
        assert_eq!(params["cvm"]["frameType"], json!("chunk"));
        assert_eq!(params["cvm"]["chunkIndex"], json!(2));
        assert_eq!(params["cvm"]["data"], json!("frag"));
        assert_eq!(params["cvm"]["type"], json!("open-stream"));
    }

    #[test]
    fn into_progress_notification_omits_absent_message() {
        let notification = OpenStreamFrame::Accept
            .into_progress_notification("tok-1", 9, None)
            .unwrap();
        let params = notification.params.as_ref().unwrap();
        assert!(!params.as_object().unwrap().contains_key("message"));
    }

    #[test]
    fn open_stream_frame_from_notification_extracts_typed_frame() {
        let notification = OpenStreamFrame::Ping {
            nonce: "n".to_string(),
        }
        .into_progress_notification("tok", 1, None)
        .unwrap();
        assert_eq!(
            open_stream_frame_from_notification(&notification),
            Some(OpenStreamFrame::Ping {
                nonce: "n".to_string()
            })
        );

        // A plain (non-cvm) progress notification yields None.
        let plain = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({ "progressToken": "t", "progress": 3 })),
        };
        assert_eq!(open_stream_frame_from_notification(&plain), None);
    }
}
