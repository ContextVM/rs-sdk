//! CEP-41 open-stream constants.
//!
//! The normative CEP leaves these numeric defaults to implementers, so we adopt
//! the TypeScript SDK's values (`sdk/src/transport/open-stream/constants.ts`)
//! for cross-implementation interop.

/// The `cvm.type` discriminator carried in every open-stream frame.
pub const OPEN_STREAM_TYPE: &str = "open-stream";

/// Default upper bound on concurrently active open streams (per peer/registry).
pub const DEFAULT_MAX_CONCURRENT_OPEN_STREAMS: usize = 64;

/// Default maximum number of buffered + queued chunks held for a single stream.
pub const DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM: usize = 64;

/// Default maximum buffered + queued payload bytes held for a single stream.
pub const DEFAULT_MAX_BUFFERED_BYTES_PER_STREAM: usize = 512 * 1024;

/// Default idle interval after which a reader probes the peer with a `ping` (milliseconds).
pub const DEFAULT_OPEN_STREAM_IDLE_TIMEOUT_MS: u64 = 30_000;

/// Default time a reader waits for a `pong` after probing before aborting (milliseconds).
pub const DEFAULT_OPEN_STREAM_PROBE_TIMEOUT_MS: u64 = 20_000;

/// Default grace period after a `close` with unresolved gaps before aborting (milliseconds).
pub const DEFAULT_OPEN_STREAM_CLOSE_GRACE_PERIOD_MS: u64 = 5_000;
