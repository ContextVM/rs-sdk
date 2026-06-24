//! FFI-safe error type.

use std::fmt;

/// Error codes returned by the FFI layer.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ErrorCode {
    /// Success (no error).
    Ok = 0,
    /// Transport / relay error.
    Transport = 1,
    /// Encryption (NIP-44) error.
    Encryption = 2,
    /// Decryption error.
    Decryption = 3,
    /// Request timed out.
    Timeout = 4,
    /// Validation error (size/schema).
    Validation = 5,
    /// Unauthorized (pubkey not in allowlist).
    Unauthorized = 6,
    /// Serialization/deserialization error.
    Serialization = 7,
    /// Generic / unknown error.
    Other = 99,
}

impl From<&contextvm_sdk::Error> for ErrorCode {
    fn from(e: &contextvm_sdk::Error) -> Self {
        match e {
            contextvm_sdk::Error::Transport(_) => ErrorCode::Transport,
            contextvm_sdk::Error::Encryption(_) => ErrorCode::Encryption,
            contextvm_sdk::Error::Decryption(_) => ErrorCode::Decryption,
            contextvm_sdk::Error::Timeout => ErrorCode::Timeout,
            contextvm_sdk::Error::Validation(_) => ErrorCode::Validation,
            contextvm_sdk::Error::Unauthorized(_) => ErrorCode::Unauthorized,
            contextvm_sdk::Error::Serialization(_) => ErrorCode::Serialization,
            // CEP-22 oversized-transfer failures are transport-layer
            // framing/reassembly issues, so they surface as CVM_TRANSPORT.
            contextvm_sdk::Error::OversizedTransfer(_) => ErrorCode::Transport,
            // CEP-41 open-stream failures (sequencing/policy/abort) are likewise
            // transport-layer, so they surface as CVM_TRANSPORT. The C header
            // exposes no dedicated code; mirrors `OversizedTransfer`.
            contextvm_sdk::Error::OpenStream(_) => ErrorCode::Transport,
            contextvm_sdk::Error::Other(_) => ErrorCode::Other,
        }
    }
}

/// An FFI-safe error with a code and human-readable message.
#[derive(Debug, Clone, uniffi::Object)]
pub struct FfiError {
    pub code: ErrorCode,
    pub message: String,
}

#[uniffi::export]
impl FfiError {
    /// Get the error code.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// Get the error message.
    pub fn message(&self) -> String {
        self.message.clone()
    }
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for FfiError {}

impl From<contextvm_sdk::Error> for FfiError {
    fn from(e: contextvm_sdk::Error) -> Self {
        FfiError {
            code: ErrorCode::from(&e),
            message: e.to_string(),
        }
    }
}

pub(crate) fn set_error(out: *mut *mut FfiError, err: FfiError) {
    if !out.is_null() {
        unsafe {
            *out = Box::into_raw(Box::new(err));
        }
    }
}
