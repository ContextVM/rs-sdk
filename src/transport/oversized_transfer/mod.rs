//! CEP-22 oversized payload transfer — transport-agnostic framing engine.
//!
//! A serialized JSON-RPC message too large to publish as a single relay event is
//! split into an ordered sequence of frames carried inside MCP
//! `notifications/progress` messages, transmitted as ordinary kind-`25910`
//! events, and reassembled by the receiver after SHA-256 + size validation.
//! See the CEP-22 spec and the TypeScript reference at
//! `sdk/src/transport/oversized-transfer/`.
//!
//! This module is the **pure engine**: building frames ([`codec`]) and
//! reassembling them ([`receiver`]). It carries no transport, I/O, or timers —
//! those are wired in by the client and server transports in later PRs. Until
//! then the module is intentionally unused by the rest of the crate.
//!
//! ```
//! use contextvm_sdk::transport::oversized_transfer::{
//!     build_oversized_frames, OversizedSenderOptions, OversizedTransferReceiver,
//! };
//! use contextvm_sdk::core::types::{JsonRpcMessage, JsonRpcResponse};
//! use serde_json::json;
//!
//! let message = JsonRpcMessage::Response(JsonRpcResponse {
//!     jsonrpc: "2.0".to_string(),
//!     id: json!(1),
//!     result: json!({ "value": "a large payload" }),
//! });
//! let serialized = serde_json::to_string(&message).unwrap();
//!
//! // Sender: split into ordered frames.
//! let opts = OversizedSenderOptions::new("token-1").with_chunk_size(8);
//! let frames = build_oversized_frames(&serialized, &opts).unwrap();
//!
//! // Receiver: feed frames back; the last frame yields the reassembled message.
//! let mut receiver = OversizedTransferReceiver::new();
//! let mut reassembled = None;
//! for frame in frames.into_ordered() {
//!     if let Some(message) = receiver.process_frame(&frame).unwrap() {
//!         reassembled = Some(message);
//!     }
//! }
//! assert_eq!(reassembled.unwrap().id(), message.id());
//! ```

pub mod codec;
pub mod constants;
pub mod errors;
pub mod frame;
pub mod receiver;

pub use codec::{
    build_oversized_frames, sha256_digest, split_string_by_byte_size, utf8_byte_len,
    BuiltOversizedFrames, OversizedSenderOptions,
};
pub use constants::*;
pub use errors::OversizedTransferError;
pub use frame::{CompletionMode, OversizedFrame};
pub use receiver::{OversizedTransferReceiver, TransferPolicy};
