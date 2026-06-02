//! Error taxonomy for CEP-22 oversized transfers.
//!
//! Mirrors the TypeScript SDK's error classes
//! (`sdk/src/transport/oversized-transfer/errors.ts`) so failure classification
//! is identical across implementations. Surfaced through the crate-level
//! [`crate::Error`] via a `#[from]` conversion.

/// Errors raised while building, framing, or reassembling an oversized transfer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OversizedTransferError {
    /// The remote peer aborted the transfer (terminal).
    #[error("Transfer aborted (token: {token}){}", .reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default())]
    Abort {
        /// The `progressToken` of the aborted transfer.
        token: String,
        /// Optional advisory reason supplied in the `abort` frame.
        reason: Option<String>,
    },

    /// A frame violated a declared or configured policy limit
    /// (`totalBytes`, `totalChunks`, concurrency, out-of-order bounds, chunk size).
    #[error("Oversized transfer policy violation: {0}")]
    Policy(String),

    /// The byte length or SHA-256 digest of the reassembled payload did not match
    /// the values declared in the `start` frame.
    #[error("Oversized transfer integrity error: {0}")]
    Digest(String),

    /// Chunks could not be reassembled (missing frames, unresolved gaps, or the
    /// reassembled payload is not a valid JSON-RPC message).
    #[error("Oversized transfer reassembly error: {0}")]
    Reassembly(String),

    /// Frames violated CEP-22 sequencing rules (bad progress ordering,
    /// duplicate `start`, conflicting duplicate chunk, missing token).
    #[error("Oversized transfer sequence error: {0}")]
    Sequence(String),
}

impl OversizedTransferError {
    /// Construct an [`OversizedTransferError::Abort`].
    pub fn abort(token: impl Into<String>, reason: Option<String>) -> Self {
        Self::Abort {
            token: token.into(),
            reason,
        }
    }

    /// Returns `true` if this is a terminal [`abort`](OversizedTransferError::Abort).
    pub fn is_abort(&self) -> bool {
        matches!(self, Self::Abort { .. })
    }
}
