//! Error types for the ContextVM SDK

/// Result type alias for ContextVM operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during ContextVM operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport-level error (relay connection, publishing, subscription)
    #[error("Transport error: {0}")]
    Transport(String),

    /// NIP-44 encryption error
    #[error("Encryption error: {0}")]
    Encryption(String),

    /// NIP-44 decryption error
    #[error("Decryption error: {0}")]
    Decryption(String),

    /// Request timed out waiting for response
    #[error("Request timed out")]
    Timeout,

    /// Message validation error (size, schema)
    #[error("Validation error: {0}")]
    Validation(String),

    /// Unauthorized request (pubkey not in allowlist)
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// CEP-22 oversized payload transfer error (framing/reassembly)
    #[error("Oversized transfer error: {0}")]
    OversizedTransfer(#[from] crate::transport::oversized_transfer::OversizedTransferError),

    /// CEP-41 open-stream error (sequencing/policy/abort)
    #[error("Open stream error: {0}")]
    OpenStream(#[from] crate::transport::open_stream::OpenStreamError),

    /// Generic error
    #[error("{0}")]
    Other(String),
}
