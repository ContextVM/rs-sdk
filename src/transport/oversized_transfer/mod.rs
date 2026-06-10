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
//! those are wired in by the client and server transports once transport
//! integration lands. Until then the module is intentionally unused by the
//! rest of the crate.
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
pub mod sender;
pub mod sizing;

pub use codec::{
    build_oversized_frames, sha256_digest, split_string_by_byte_size, utf8_byte_len,
    BuiltOversizedFrames, OversizedSenderOptions,
};
pub use constants::*;
pub use errors::OversizedTransferError;
pub use frame::{CompletionMode, OversizedFrame};
pub use receiver::{OversizedTransferReceiver, TransferPolicy};
pub use sender::send_oversized_transfer;
pub use sizing::{measure_published_event_size, resolve_safe_chunk_size};

/// CEP-22 oversized-transfer configuration shared by both transports.
///
/// Bundles the capability gate plus the sender/receiver tuning knobs (D6) so the
/// nine numeric defaults don't clutter the flat transport configs. Attached to
/// [`NostrServerTransportConfig`](crate::transport::NostrServerTransportConfig)
/// and [`NostrClientTransportConfig`](crate::transport::NostrClientTransportConfig)
/// via their `with_oversized_transfer` / `with_oversized_enabled` builders.
///
/// **Disabled by default** — until a peer opts in, no `support_oversized_transfer`
/// capability is advertised and the server never learns or activates the feature.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OversizedTransferConfig {
    /// Master gate. When `false` (default) the capability is neither advertised
    /// nor activated, and the server does not learn a client's flag.
    pub enabled: bool,
    /// Serialized byte length at or above which the sender switches to oversized transfer.
    pub threshold: usize,
    /// Per-chunk data size (bytes).
    pub chunk_size: usize,
    /// Upper bound on the total reassembled payload a receiver will accept (bytes).
    pub max_transfer_bytes: u64,
    /// Upper bound on the number of chunks a receiver will accept.
    pub max_transfer_chunks: u64,
    /// Upper bound on concurrently active receiver-side transfers.
    pub max_concurrent_transfers: usize,
    /// Hard timeout for an in-flight transfer (milliseconds).
    pub transfer_timeout_ms: u64,
    /// Maximum forward gap between the next expected chunk and an out-of-order
    /// chunk that will still be buffered.
    pub max_out_of_order_window: u64,
    /// Maximum number of buffered out-of-order chunks.
    pub max_out_of_order_chunks: usize,
    /// Timeout a sender waits for an `accept` frame before giving up (milliseconds).
    pub accept_timeout_ms: u64,
}

impl Default for OversizedTransferConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: DEFAULT_OVERSIZED_THRESHOLD,
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_transfer_bytes: DEFAULT_MAX_TRANSFER_BYTES,
            max_transfer_chunks: DEFAULT_MAX_TRANSFER_CHUNKS,
            max_concurrent_transfers: DEFAULT_MAX_CONCURRENT_TRANSFERS,
            transfer_timeout_ms: DEFAULT_TRANSFER_TIMEOUT_MS,
            max_out_of_order_window: DEFAULT_MAX_OUT_OF_ORDER_WINDOW,
            max_out_of_order_chunks: DEFAULT_MAX_OUT_OF_ORDER_CHUNKS,
            accept_timeout_ms: DEFAULT_ACCEPT_TIMEOUT_MS,
        }
    }
}

impl OversizedTransferConfig {
    /// An explicitly enabled config with all other knobs at their defaults.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Enable or disable oversized transfer.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Set the serialized-byte threshold at which the sender fragments.
    pub fn with_threshold(mut self, threshold: usize) -> Self {
        self.threshold = threshold;
        self
    }

    /// Set the per-chunk data size (bytes).
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Set the upper bound on the total reassembled payload (bytes).
    pub fn with_max_transfer_bytes(mut self, max: u64) -> Self {
        self.max_transfer_bytes = max;
        self
    }

    /// Set the upper bound on the number of chunks a receiver will accept.
    pub fn with_max_transfer_chunks(mut self, max: u64) -> Self {
        self.max_transfer_chunks = max;
        self
    }

    /// Set the upper bound on concurrently active receiver-side transfers.
    pub fn with_max_concurrent_transfers(mut self, max: usize) -> Self {
        self.max_concurrent_transfers = max;
        self
    }

    /// Set the hard per-transfer timeout (milliseconds).
    pub fn with_transfer_timeout_ms(mut self, ms: u64) -> Self {
        self.transfer_timeout_ms = ms;
        self
    }

    /// Set the maximum forward gap for buffering out-of-order chunks.
    pub fn with_max_out_of_order_window(mut self, window: u64) -> Self {
        self.max_out_of_order_window = window;
        self
    }

    /// Set the maximum number of buffered out-of-order chunks.
    pub fn with_max_out_of_order_chunks(mut self, max: usize) -> Self {
        self.max_out_of_order_chunks = max;
        self
    }

    /// Set the sender's `accept`-frame wait timeout (milliseconds).
    pub fn with_accept_timeout_ms(mut self, ms: u64) -> Self {
        self.accept_timeout_ms = ms;
        self
    }
}

impl From<&OversizedTransferConfig> for TransferPolicy {
    /// Project the receiver-relevant knobs of an [`OversizedTransferConfig`] into
    /// a [`TransferPolicy`] (D6 → receiver admission policy).
    fn from(config: &OversizedTransferConfig) -> Self {
        TransferPolicy {
            max_transfer_bytes: config.max_transfer_bytes,
            max_transfer_chunks: config.max_transfer_chunks,
            max_concurrent_transfers: config.max_concurrent_transfers,
            max_out_of_order_window: config.max_out_of_order_window,
            max_out_of_order_chunks: config.max_out_of_order_chunks,
            transfer_timeout_ms: config.transfer_timeout_ms,
        }
    }
}
