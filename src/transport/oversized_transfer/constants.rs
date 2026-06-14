//! CEP-22 oversized-transfer constants.
//!
//! The normative CEP leaves these numeric defaults to implementers, so we adopt
//! the TypeScript SDK's values (`sdk/src/transport/oversized-transfer/constants.ts`)
//! for cross-implementation interop.

/// The `cvm.type` discriminator carried in every oversized-transfer frame.
pub const OVERSIZED_TRANSFER_TYPE: &str = "oversized-transfer";

/// Prefix applied to SHA-256 digest values (lowercase hex follows).
pub const DIGEST_PREFIX: &str = "sha256:";

/// JSON-RPC method that carries oversized-transfer frames in `params.cvm`.
pub const NOTIFICATIONS_PROGRESS_METHOD: &str = "notifications/progress";

/// Default per-chunk data size (bytes).
///
/// Conservative: leaves ~16 KiB of headroom under the ~64 KiB relay event
/// threshold so a single gift-wrapped frame stays well below it.
pub const DEFAULT_CHUNK_SIZE: usize = 48_000;

/// Serialized byte length at or above which the sender switches to oversized transfer.
pub const DEFAULT_OVERSIZED_THRESHOLD: usize = 48_000;

/// Default upper bound on the total reassembled payload a receiver will accept (100 MiB).
pub const DEFAULT_MAX_TRANSFER_BYTES: u64 = 100 * 1024 * 1024;

/// Default upper bound on the number of chunks a receiver will accept.
pub const DEFAULT_MAX_TRANSFER_CHUNKS: u64 = 10_000;

/// Default upper bound on concurrently active receiver-side transfers.
pub const DEFAULT_MAX_CONCURRENT_TRANSFERS: usize = 64;

/// Default hard timeout for an in-flight transfer (milliseconds).
///
/// Measured from `start` admission, never refreshed by chunk activity, and
/// enforced by sweeping
/// [`OversizedTransferReceiver::remove_expired`](super::OversizedTransferReceiver::remove_expired).
pub const DEFAULT_TRANSFER_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// Default maximum forward gap between the next expected chunk and an
/// out-of-order chunk that will still be buffered.
pub const DEFAULT_MAX_OUT_OF_ORDER_WINDOW: u64 = 21;

/// Default maximum number of buffered out-of-order chunks.
pub const DEFAULT_MAX_OUT_OF_ORDER_CHUNKS: usize = 42;

/// Default timeout a sender waits for an `accept` frame before giving up (milliseconds).
///
/// Used by the sender handshake once transport integration lands; defined here for parity.
pub const DEFAULT_ACCEPT_TIMEOUT_MS: u64 = 30_000;

/// Canonical progress slot for the `start` frame.
pub const START_PROGRESS: u64 = 1;

/// Progress slot reserved for the `accept` frame.
///
/// In the handshake case the `accept` frame occupies this slot and the first
/// `chunk` starts at [`START_PROGRESS`] + 2. In the no-handshake case no
/// `accept` is sent, so this slot is reused as the first `chunk`.
pub const ACCEPT_PROGRESS: u64 = 2;
