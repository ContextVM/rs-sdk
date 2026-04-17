//! Lightweight logging helpers that wrap the `tracing` crate.
//!
//! These exist to provide a logger abstraction similar to the TS SDK's
//! `createLogger()` utility while using Rust-idiomatic structured logging.

/// Log an error message with a specific target path.
///
/// Note: `tracing`'s `target:` parameter must be a `&'static str` literal at
/// compile time.  We embed the caller-supplied target as a structured field
/// instead so that it appears in log output without violating macro constraints.
pub fn error_with_target(target: &str, message: impl std::fmt::Display) {
    tracing::error!(log_target = target, "{message}");
}
