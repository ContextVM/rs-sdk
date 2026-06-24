//! CEP-41 open-ended streaming — transport-agnostic engine.
//!
//! A server tool can emit an ordered, unbounded sequence of `chunk` fragments
//! back to a client *while a request is in flight*; the client consumes them
//! incrementally as an async [`Stream`](futures::Stream). Unlike CEP-22, the
//! stream does **not** replace the final JSON-RPC response — one `tools/call`
//! produces two outputs: a live `notifications/progress` stream and the normal
//! final response that still concludes the request.
//!
//! Frames ride inside `notifications/progress` notifications on the existing
//! `CTXVM_MESSAGES_KIND`, discriminated by `params.cvm.type == "open-stream"`
//! ([`frame`]). The stream id is the request `progressToken`. See the CEP-41
//! spec and the TypeScript reference at `sdk/src/transport/open-stream/`.
//!
//! This module is the **pure engine**: framing ([`frame`]), the reader
//! [`OpenStreamSession`] (incl. the pure keepalive [`tick`](OpenStreamSession::tick)),
//! the producer [`OpenStreamWriter`], and the per-peer [`OpenStreamRegistry`].
//! It carries no transport or live timers — the client and server transports
//! drive it (the keepalive sweep calls `tick`; inbound frames feed
//! `process_frame`).

pub mod constants;
pub mod errors;
pub mod frame;
pub mod receiver;
pub mod registry;
pub mod session;
pub mod writer;

pub use constants::*;
pub use errors::OpenStreamError;
pub use frame::{open_stream_frame_from_notification, OpenStreamFrame};
pub use receiver::OpenStreamReceiver;
pub use registry::{
    OpenStreamRegistry, OpenStreamRegistryPolicy, OpenStreamSessionInit, RegistryAbortHook,
    RegistryCloseHook,
};
pub use session::{
    FrameOutcome, KeepaliveAction, OpenStreamSession, OpenStreamSessionOptions, PublishFrame,
};
pub use writer::{OnAbortHook, OnCloseHook, OpenStreamWriter, OpenStreamWriterOptions};

/// CEP-41 open-stream configuration shared by both transports.
///
/// Bundles the capability gate plus the reader buffering/keepalive knobs.
/// Attached to
/// [`NostrServerTransportConfig`](crate::transport::NostrServerTransportConfig)
/// and [`NostrClientTransportConfig`](crate::transport::NostrClientTransportConfig)
/// via their `with_open_stream` builders.
///
/// **Disabled by default** (opt-in): open-stream is neither advertised nor
/// activated until enabled (matching the TS SDK's default). Opt in with
/// [`OpenStreamConfig::enabled`] or `with_enabled(true)`. Once on, it is safe for
/// non-CEP-41 peers — the server activates only for advertising clients, and a
/// writer is injected only when a request carries a `progressToken`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OpenStreamConfig {
    /// Master gate, `false` by default. When `false` the capability is neither
    /// advertised nor activated, and the server does not learn a client's flag.
    pub enabled: bool,
    /// Upper bound on concurrently active streams (per peer/registry).
    pub max_concurrent_streams: usize,
    /// Upper bound on buffered + queued chunks held for a single stream.
    pub max_buffered_chunks_per_stream: usize,
    /// Upper bound on buffered + queued payload bytes held for a single stream.
    pub max_buffered_bytes_per_stream: usize,
    /// Idle interval after which a reader probes the peer with a `ping` (ms).
    pub idle_timeout_ms: u64,
    /// Time a reader waits for a `pong` after probing before aborting (ms).
    pub probe_timeout_ms: u64,
    /// Grace period after a `close` with unresolved gaps before aborting (ms).
    pub close_grace_period_ms: u64,
    /// Optional hard cap on total stream lifetime (ms).
    ///
    /// `None` (the default) means no lifetime cap. When set, only
    /// `call_tool_stream` reads it (the registry and keepalive sweep never do);
    /// it is **not** the CEP-22 `DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT`.
    pub max_total_timeout_ms: Option<u64>,
}

impl Default for OpenStreamConfig {
    fn default() -> Self {
        Self {
            // Open-stream is opt-in (disabled by default), matching the TS SDK.
            // Enable it with `OpenStreamConfig::enabled()` / `with_enabled(true)`.
            enabled: false,
            max_concurrent_streams: constants::DEFAULT_MAX_CONCURRENT_OPEN_STREAMS,
            max_buffered_chunks_per_stream: constants::DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM,
            max_buffered_bytes_per_stream: constants::DEFAULT_MAX_BUFFERED_BYTES_PER_STREAM,
            idle_timeout_ms: constants::DEFAULT_OPEN_STREAM_IDLE_TIMEOUT_MS,
            probe_timeout_ms: constants::DEFAULT_OPEN_STREAM_PROBE_TIMEOUT_MS,
            close_grace_period_ms: constants::DEFAULT_OPEN_STREAM_CLOSE_GRACE_PERIOD_MS,
            max_total_timeout_ms: None,
        }
    }
}

impl OpenStreamConfig {
    /// An explicitly enabled config with all other knobs at their defaults.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Enable or disable open-stream support.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Set the upper bound on concurrently active streams.
    pub fn with_max_concurrent_streams(mut self, max: usize) -> Self {
        self.max_concurrent_streams = max;
        self
    }

    /// Set the upper bound on buffered + queued chunks per stream.
    pub fn with_max_buffered_chunks_per_stream(mut self, max: usize) -> Self {
        self.max_buffered_chunks_per_stream = max;
        self
    }

    /// Set the upper bound on buffered + queued payload bytes per stream.
    pub fn with_max_buffered_bytes_per_stream(mut self, max: usize) -> Self {
        self.max_buffered_bytes_per_stream = max;
        self
    }

    /// Set the idle interval before a reader probes with a `ping` (ms).
    pub fn with_idle_timeout_ms(mut self, ms: u64) -> Self {
        self.idle_timeout_ms = ms;
        self
    }

    /// Set the time a reader waits for a `pong` before aborting (ms).
    pub fn with_probe_timeout_ms(mut self, ms: u64) -> Self {
        self.probe_timeout_ms = ms;
        self
    }

    /// Set the close grace period before aborting on unresolved gaps (ms).
    pub fn with_close_grace_period_ms(mut self, ms: u64) -> Self {
        self.close_grace_period_ms = ms;
        self
    }

    /// Set the optional hard cap on total stream lifetime (ms).
    pub fn with_max_total_timeout_ms(mut self, ms: Option<u64>) -> Self {
        self.max_total_timeout_ms = ms;
        self
    }
}

impl From<&OpenStreamConfig> for OpenStreamRegistryPolicy {
    /// Project the registry-relevant knobs of an [`OpenStreamConfig`] into an
    /// [`OpenStreamRegistryPolicy`] (the reader admission/buffering policy).
    fn from(config: &OpenStreamConfig) -> Self {
        OpenStreamRegistryPolicy {
            max_concurrent_streams: config.max_concurrent_streams,
            max_buffered_chunks_per_stream: config.max_buffered_chunks_per_stream,
            max_buffered_bytes_per_stream: config.max_buffered_bytes_per_stream,
            idle_timeout_ms: config.idle_timeout_ms,
            probe_timeout_ms: config.probe_timeout_ms,
            close_grace_period_ms: config.close_grace_period_ms,
        }
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn default_is_disabled_with_ts_parity_knobs() {
        let config = OpenStreamConfig::default();
        // Open-stream is opt-in (disabled by default), matching the TS SDK.
        assert!(!config.enabled);
        // Opting in is one call.
        assert!(OpenStreamConfig::default().with_enabled(true).enabled);
        assert!(OpenStreamConfig::enabled().enabled);
        assert_eq!(config.max_concurrent_streams, 64);
        assert_eq!(config.max_buffered_chunks_per_stream, 64);
        assert_eq!(config.max_buffered_bytes_per_stream, 512 * 1024);
        assert_eq!(config.idle_timeout_ms, 30_000);
        assert_eq!(config.probe_timeout_ms, 20_000);
        assert_eq!(config.close_grace_period_ms, 5_000);
        // No hard lifetime cap by default.
        assert_eq!(config.max_total_timeout_ms, None);
    }

    #[test]
    fn builders_opt_in_and_override() {
        let config = OpenStreamConfig::default()
            .with_enabled(true)
            .with_max_concurrent_streams(8)
            .with_max_buffered_bytes_per_stream(1024)
            .with_max_total_timeout_ms(Some(60_000));
        assert!(config.enabled);
        assert_eq!(config.max_concurrent_streams, 8);
        assert_eq!(config.max_buffered_bytes_per_stream, 1024);
        assert_eq!(config.max_total_timeout_ms, Some(60_000));

        assert!(OpenStreamConfig::enabled().enabled);
    }

    #[test]
    fn projects_into_registry_policy() {
        let config = OpenStreamConfig::default()
            .with_max_concurrent_streams(3)
            .with_max_buffered_chunks_per_stream(5)
            .with_max_buffered_bytes_per_stream(7)
            .with_idle_timeout_ms(11)
            .with_probe_timeout_ms(13)
            .with_close_grace_period_ms(17);
        let policy: OpenStreamRegistryPolicy = (&config).into();
        assert_eq!(policy.max_concurrent_streams, 3);
        assert_eq!(policy.max_buffered_chunks_per_stream, 5);
        assert_eq!(policy.max_buffered_bytes_per_stream, 7);
        assert_eq!(policy.idle_timeout_ms, 11);
        assert_eq!(policy.probe_timeout_ms, 13);
        assert_eq!(policy.close_grace_period_ms, 17);
    }
}
