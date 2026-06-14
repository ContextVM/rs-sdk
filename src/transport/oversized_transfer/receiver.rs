//! Stateful reassembly engine for CEP-22 oversized transfers.
//!
//! Ports `sdk/src/transport/oversized-transfer/receiver.ts`. Feeds inbound
//! `notifications/progress` frames through [`OversizedTransferReceiver::process_frame`]
//! and returns the reassembled [`JsonRpcMessage`] once a transfer completes and
//! passes byte-length, SHA-256, and JSON-RPC validation.
//!
//! This engine is **pure and synchronous**: it owns no live timers. The hard
//! per-transfer watchdog deadline (`transfer_timeout_ms`) is measured from
//! `start` admission and enforced by the owning transport calling
//! [`OversizedTransferReceiver::remove_expired`] on its sweep tick; the
//! sender-side accept-waiter lives in the transport.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::types::{JsonRpcMessage, JsonRpcNotification};
use crate::core::validation::validate_and_parse_oversized;

use super::codec::sha256_digest;
use super::constants::{
    DEFAULT_MAX_CONCURRENT_TRANSFERS, DEFAULT_MAX_OUT_OF_ORDER_CHUNKS,
    DEFAULT_MAX_OUT_OF_ORDER_WINDOW, DEFAULT_MAX_TRANSFER_BYTES, DEFAULT_MAX_TRANSFER_CHUNKS,
    DEFAULT_TRANSFER_TIMEOUT_MS, DIGEST_PREFIX,
};
use super::errors::OversizedTransferError;
use super::frame::{progress_token_string, OversizedFrame};

/// Receiver-side admission and out-of-order policy.
///
/// Mirrors the TS `TransferPolicy`. Construct via [`TransferPolicy::default`]
/// and override individual fields with struct-update syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferPolicy {
    /// Maximum total reassembled payload size accepted (bytes).
    pub max_transfer_bytes: u64,
    /// Maximum number of chunks accepted.
    pub max_transfer_chunks: u64,
    /// Maximum number of concurrently active transfers.
    pub max_concurrent_transfers: usize,
    /// Maximum forward gap from the contiguous frontier that is still buffered.
    pub max_out_of_order_window: u64,
    /// Maximum number of buffered out-of-order chunks.
    pub max_out_of_order_chunks: usize,
    /// Hard per-transfer timeout (milliseconds), measured from `start`
    /// admission and never refreshed by chunk activity (TS parity).
    /// Enforced by sweeping [`OversizedTransferReceiver::remove_expired`];
    /// `0` disables the watchdog.
    pub transfer_timeout_ms: u64,
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            max_transfer_bytes: DEFAULT_MAX_TRANSFER_BYTES,
            max_transfer_chunks: DEFAULT_MAX_TRANSFER_CHUNKS,
            max_concurrent_transfers: DEFAULT_MAX_CONCURRENT_TRANSFERS,
            max_out_of_order_window: DEFAULT_MAX_OUT_OF_ORDER_WINDOW,
            max_out_of_order_chunks: DEFAULT_MAX_OUT_OF_ORDER_CHUNKS,
            transfer_timeout_ms: DEFAULT_TRANSFER_TIMEOUT_MS,
        }
    }
}

/// In-flight state for a single transfer, keyed by `progressToken`.
#[derive(Debug)]
struct ActiveTransfer {
    digest: String,
    total_bytes: u64,
    total_chunks: u64,
    start_progress: u64,
    accept_progress: Option<u64>,
    first_chunk_progress: Option<u64>,
    next_expected_chunk_progress: Option<u64>,
    highest_observed_progress: u64,
    /// Chunk fragments keyed by the outer `progress` value (canonical index).
    chunks: BTreeMap<u64, String>,
    /// When the `start` frame was admitted. The watchdog deadline is measured
    /// from here and never refreshed (a hard cap, not an idle timer).
    admitted_at: Instant,
}

impl ActiveTransfer {
    /// The progress slot the first chunk occupies, given handshake state.
    fn first_chunk_slot(&self) -> u64 {
        if self.accept_progress.is_some() {
            self.start_progress + 2
        } else {
            self.start_progress + 1
        }
    }

    /// The next contiguous chunk slot the assembler is waiting for.
    fn next_expected_chunk_progress(&self) -> u64 {
        self.next_expected_chunk_progress
            .unwrap_or_else(|| self.first_chunk_slot())
    }

    /// Recompute the contiguous-frontier bookkeeping after a chunk insert.
    fn refresh_chunk_progress_state(&mut self) {
        if self.chunks.is_empty() {
            self.first_chunk_progress = None;
            self.next_expected_chunk_progress = None;
            return;
        }

        let first = self.first_chunk_slot();
        let mut next = first;
        while self.chunks.contains_key(&next) {
            next += 1;
        }

        self.first_chunk_progress = Some(first);
        self.next_expected_chunk_progress = Some(next);
    }

    /// Count of buffered chunks sitting beyond the contiguous frontier.
    fn buffered_out_of_order_count(&self) -> usize {
        match (self.first_chunk_progress, self.next_expected_chunk_progress) {
            (Some(first), Some(next)) => self.chunks.len() - (next - first) as usize,
            _ => 0,
        }
    }

    /// Whether every slot in `[first, first + total_chunks)` is present.
    fn has_complete_chunk_range(&self, first: u64) -> bool {
        (0..self.total_chunks).all(|i| self.chunks.contains_key(&(first + i)))
    }

    /// Resolve the first chunk slot for assembly, tolerating the reserved accept
    /// slot being used or skipped. Returns `None` if neither layout is complete.
    fn assembly_first_chunk_progress(&self) -> Option<u64> {
        let direct = self.start_progress + 1;
        let accept_gated = self.start_progress + 2;
        if self.has_complete_chunk_range(direct) {
            Some(direct)
        } else if self.has_complete_chunk_range(accept_gated) {
            Some(accept_gated)
        } else {
            None
        }
    }

    /// Concatenate chunks in `progress` order. `None` on any unresolved gap.
    fn assemble(&self) -> Option<String> {
        let first = self.assembly_first_chunk_progress()?;
        let mut out = String::with_capacity(self.total_bytes as usize);
        for i in 0..self.total_chunks {
            out.push_str(self.chunks.get(&(first + i))?);
        }
        Some(out)
    }
}

/// Stateful CEP-22 reassembly engine.
///
/// Tracks per-`progressToken` transfer state, enforces admission and
/// out-of-order policy, and validates integrity before surfacing a reassembled
/// message. Never surfaces partial payloads.
#[derive(Debug)]
pub struct OversizedTransferReceiver {
    max_transfer_bytes: u64,
    max_transfer_chunks: u64,
    max_concurrent_transfers: usize,
    max_out_of_order_window: u64,
    max_out_of_order_chunks: usize,
    /// Hard per-transfer deadline in milliseconds, from `start` admission.
    /// `0` disables the watchdog: [`Self::remove_expired`] skips the sweep
    /// entirely rather than expiring transfers instantly.
    transfer_timeout_ms: u64,
    transfers: HashMap<String, ActiveTransfer>,
}

impl Default for OversizedTransferReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl OversizedTransferReceiver {
    /// Create a receiver with the default [`TransferPolicy`].
    pub fn new() -> Self {
        Self::with_policy(TransferPolicy::default())
    }

    /// Create a receiver with an explicit policy.
    pub fn with_policy(policy: TransferPolicy) -> Self {
        Self {
            max_transfer_bytes: policy.max_transfer_bytes,
            max_transfer_chunks: policy.max_transfer_chunks,
            max_concurrent_transfers: policy.max_concurrent_transfers,
            max_out_of_order_window: policy.max_out_of_order_window,
            max_out_of_order_chunks: policy.max_out_of_order_chunks,
            transfer_timeout_ms: policy.transfer_timeout_ms,
            transfers: HashMap::new(),
        }
    }

    /// Number of currently active in-flight transfers.
    pub fn active_transfer_count(&self) -> usize {
        self.transfers.len()
    }

    /// Whether a transfer keyed by `token` is currently in flight.
    ///
    /// Lets the transport gate progress forwarding on tracked transfers, so a
    /// late or orphan frame (e.g. after watchdog reaping) cannot keep a dead
    /// request's timer alive.
    pub fn is_tracking(&self, token: &str) -> bool {
        self.transfers.contains_key(token)
    }

    /// Reap transfers whose age since `start` admission exceeds the policy's
    /// `transfer_timeout_ms`, returning the reaped tokens (for logging).
    ///
    /// The deadline is hard — never refreshed by chunk activity (TS parity):
    /// liveness is the requester's idle timer; this sweep is the receiver's
    /// memory bound. A `transfer_timeout_ms` of `0` disables the watchdog (no
    /// sweep). Reaping is local-only — no abort frame is emitted; the peer's
    /// own timeout covers the other side. The token slot is freed: a later
    /// `start` re-using a reaped token is admitted as a fresh transfer, while
    /// its late chunk/end frames are orphan-ignored.
    pub fn remove_expired(&mut self) -> Vec<String> {
        if self.transfer_timeout_ms == 0 {
            return Vec::new();
        }
        let deadline = Duration::from_millis(self.transfer_timeout_ms);
        let now = Instant::now();
        let mut reaped = Vec::new();
        self.transfers.retain(|token, transfer| {
            if now.duration_since(transfer.admitted_at) > deadline {
                reaped.push(token.clone());
                false
            } else {
                true
            }
        });
        reaped
    }

    /// Release all in-flight transfer state.
    pub fn clear(&mut self) {
        self.transfers.clear();
    }

    /// Returns `true` when `notification` carries an oversized-transfer frame in
    /// its `params.cvm` field.
    pub fn is_oversized_frame(notification: &JsonRpcNotification) -> bool {
        notification
            .params
            .as_ref()
            .and_then(|params| params.get("cvm"))
            .map(OversizedFrame::is_frame_value)
            .unwrap_or(false)
    }

    /// Process one inbound `notifications/progress` frame.
    ///
    /// Returns `Ok(Some(message))` once the transfer is complete and validated,
    /// `Ok(None)` when more frames are needed (or the notification is not an
    /// oversized frame), and `Err(..)` on abort, policy, integrity, sequence, or
    /// reassembly failure. A failing or aborting transfer is cleaned up before
    /// the error is returned.
    pub fn process_frame(
        &mut self,
        notification: &JsonRpcNotification,
    ) -> Result<Option<JsonRpcMessage>, OversizedTransferError> {
        let params = match notification.params.as_ref() {
            Some(params) => params,
            None => return Ok(None),
        };
        let cvm = match params.get("cvm") {
            Some(cvm) => cvm,
            None => return Ok(None),
        };
        let frame = match OversizedFrame::from_cvm_value(cvm) {
            Some(frame) => frame,
            None => return Ok(None),
        };

        // The outer progressToken keys the transfer; the outer progress is the
        // canonical ordering index. Both are validated before dispatch.
        let token = token_to_string(params.get("progressToken"));
        assert_valid_token(&token)?;
        let progress = parse_progress(params.get("progress"), &token)?;

        match frame {
            OversizedFrame::Start {
                digest,
                total_bytes,
                total_chunks,
                ..
            } => self.handle_start(&token, progress, digest, total_bytes, total_chunks),
            OversizedFrame::Accept => self.handle_accept(&token, progress),
            OversizedFrame::Chunk { data } => self.handle_chunk(&token, progress, data),
            OversizedFrame::End => self.handle_end(&token, progress),
            OversizedFrame::Abort { reason } => self.handle_abort(&token, reason),
        }
    }

    fn handle_start(
        &mut self,
        token: &str,
        progress: u64,
        digest: String,
        total_bytes: u64,
        total_chunks: u64,
    ) -> Result<Option<JsonRpcMessage>, OversizedTransferError> {
        if self.transfers.contains_key(token) {
            return Err(OversizedTransferError::Sequence(format!(
                "Duplicate start frame for active transfer (token: {token})"
            )));
        }
        if total_bytes > self.max_transfer_bytes {
            return Err(OversizedTransferError::Policy(format!(
                "totalBytes {total_bytes} exceeds policy limit {} (token: {token})",
                self.max_transfer_bytes
            )));
        }
        if total_chunks > self.max_transfer_chunks {
            return Err(OversizedTransferError::Policy(format!(
                "totalChunks {total_chunks} exceeds policy limit {} (token: {token})",
                self.max_transfer_chunks
            )));
        }
        if self.transfers.len() >= self.max_concurrent_transfers {
            return Err(OversizedTransferError::Policy(format!(
                "Active transfers exceed policy limit {} (token: {token})",
                self.max_concurrent_transfers
            )));
        }
        if !digest.starts_with(DIGEST_PREFIX) {
            return Err(OversizedTransferError::Reassembly(format!(
                "Invalid digest format in start frame (token: {token})"
            )));
        }

        self.transfers.insert(
            token.to_string(),
            ActiveTransfer {
                digest,
                total_bytes,
                total_chunks,
                start_progress: progress,
                accept_progress: None,
                first_chunk_progress: None,
                next_expected_chunk_progress: None,
                highest_observed_progress: progress,
                chunks: BTreeMap::new(),
                admitted_at: Instant::now(),
            },
        );
        Ok(None)
    }

    fn handle_accept(
        &mut self,
        token: &str,
        progress: u64,
    ) -> Result<Option<JsonRpcMessage>, OversizedTransferError> {
        // Dropping `transfer` on the error path is the cleanup (failTransfer).
        let mut transfer = match self.transfers.remove(token) {
            Some(transfer) => transfer,
            // Late or duplicated accept frames are ignored after cleanup.
            None => return Ok(None),
        };

        if progress <= transfer.start_progress {
            return Err(OversizedTransferError::Sequence(format!(
                "Accept frame progress must be greater than start progress (token: {token})"
            )));
        }

        transfer.highest_observed_progress = transfer.highest_observed_progress.max(progress);
        transfer.accept_progress = Some(progress);
        self.transfers.insert(token.to_string(), transfer);
        Ok(None)
    }

    fn handle_chunk(
        &mut self,
        token: &str,
        progress: u64,
        data: String,
    ) -> Result<Option<JsonRpcMessage>, OversizedTransferError> {
        let max_window = self.max_out_of_order_window;
        let max_ooo_chunks = self.max_out_of_order_chunks;

        let mut transfer = match self.transfers.remove(token) {
            Some(transfer) => transfer,
            // Late or duplicated chunk frames are ignored after cleanup.
            None => return Ok(None),
        };

        let minimum_chunk_progress = transfer.start_progress + 1;
        let maximum_chunk_progress = transfer.start_progress + transfer.total_chunks + 1;

        if progress < minimum_chunk_progress {
            return Err(OversizedTransferError::Sequence(format!(
                "Chunk progress must be greater than start progress (token: {token})"
            )));
        }

        let next_expected = transfer.next_expected_chunk_progress();
        // i128 so a hostile `progress` near u64::MAX cannot overflow the subtraction.
        let forward_gap = i128::from(progress) - i128::from(next_expected);
        if forward_gap > i128::from(max_window) {
            return Err(OversizedTransferError::Policy(format!(
                "Out-of-order gap {forward_gap} exceeds policy limit {max_window} (token: {token})"
            )));
        }

        if progress > maximum_chunk_progress {
            return Err(OversizedTransferError::Sequence(format!(
                "Chunk progress exceeds declared transfer bounds (token: {token})"
            )));
        }

        if progress > transfer.start_progress + 2
            && !transfer.chunks.contains_key(&(transfer.start_progress + 1))
            && !transfer.chunks.contains_key(&(transfer.start_progress + 2))
        {
            return Err(OversizedTransferError::Sequence(format!(
                "First chunk skips beyond the reserved accept slot (token: {token})"
            )));
        }

        if let Some(existing) = transfer.chunks.get(&progress) {
            if existing != &data {
                return Err(OversizedTransferError::Sequence(format!(
                    "Conflicting duplicate chunk detected (token: {token}, progress: {progress})"
                )));
            }
            // Idempotent identical duplicate: keep state, await more frames.
            self.transfers.insert(token.to_string(), transfer);
            return Ok(None);
        }

        transfer.chunks.insert(progress, data);
        transfer.highest_observed_progress = transfer.highest_observed_progress.max(progress);
        transfer.refresh_chunk_progress_state();

        if forward_gap > 0 && transfer.buffered_out_of_order_count() > max_ooo_chunks {
            return Err(OversizedTransferError::Policy(format!(
                "Buffered out-of-order chunks exceed policy limit {max_ooo_chunks} (token: {token})"
            )));
        }

        self.transfers.insert(token.to_string(), transfer);
        Ok(None)
    }

    fn handle_end(
        &mut self,
        token: &str,
        progress: u64,
    ) -> Result<Option<JsonRpcMessage>, OversizedTransferError> {
        let max_bytes = self.max_transfer_bytes;
        // Removed up front: every end-path outcome (success or failure) is terminal.
        let transfer = match self.transfers.remove(token) {
            Some(transfer) => transfer,
            // Late or duplicated end frames are ignored after cleanup.
            None => return Ok(None),
        };

        if progress <= transfer.highest_observed_progress {
            return Err(OversizedTransferError::Sequence(format!(
                "End frame progress must be greater than all prior transfer frames (token: {token})"
            )));
        }
        if transfer.total_chunks > 0 && transfer.chunks.is_empty() {
            return Err(OversizedTransferError::Reassembly(format!(
                "Transfer ended before any chunks were received (token: {token})"
            )));
        }
        if transfer.chunks.len() as u64 != transfer.total_chunks {
            return Err(OversizedTransferError::Reassembly(format!(
                "Expected {} chunks but received {} (token: {token})",
                transfer.total_chunks,
                transfer.chunks.len()
            )));
        }

        let assembled = match transfer.assemble() {
            Some(assembled) => assembled,
            None => {
                return Err(OversizedTransferError::Reassembly(format!(
                    "Transfer ended with unresolved chunk gaps (token: {token})"
                )))
            }
        };

        // 1. Byte-length validation.
        if assembled.len() as u64 != transfer.total_bytes {
            return Err(OversizedTransferError::Digest(format!(
                "Byte length mismatch: expected {}, got {} (token: {token})",
                transfer.total_bytes,
                assembled.len()
            )));
        }

        // 2. SHA-256 digest validation.
        if sha256_digest(&assembled) != transfer.digest {
            return Err(OversizedTransferError::Digest(format!(
                "SHA-256 digest mismatch (token: {token})"
            )));
        }

        // 3. Parse + validate as a JSON-RPC message, bypassing the 1 MB
        //    per-event cap in favor of the receiver policy's maxTransferBytes.
        match validate_and_parse_oversized(&assembled, max_bytes as usize) {
            Some(message) => Ok(Some(message)),
            None => Err(OversizedTransferError::Reassembly(format!(
                "Reassembled payload is not a valid JSON-RPC message (token: {token})"
            ))),
        }
    }

    fn handle_abort(
        &mut self,
        token: &str,
        reason: Option<String>,
    ) -> Result<Option<JsonRpcMessage>, OversizedTransferError> {
        // Abort is terminal whether or not a transfer is currently tracked.
        self.transfers.remove(token);
        Err(OversizedTransferError::abort(token, reason))
    }
}

/// Coerce a `progressToken` value to a string (mirrors TS `String(token ?? '')`).
fn token_to_string(value: Option<&Value>) -> String {
    value.and_then(progress_token_string).unwrap_or_default()
}

/// A non-empty token is required on every frame.
fn assert_valid_token(token: &str) -> Result<(), OversizedTransferError> {
    if token.is_empty() {
        return Err(OversizedTransferError::Sequence(
            "Oversized transfer frame is missing progressToken".to_string(),
        ));
    }
    Ok(())
}

/// Parse and validate the outer `progress`: a positive integer.
fn parse_progress(value: Option<&Value>, token: &str) -> Result<u64, OversizedTransferError> {
    let progress = match value {
        Some(Value::Number(n)) => n.as_u64().or_else(|| {
            n.as_f64().and_then(|f| {
                if f.fract() == 0.0 && f > 0.0 {
                    Some(f as u64)
                } else {
                    None
                }
            })
        }),
        _ => None,
    };

    match progress {
        Some(progress) if progress > 0 => Ok(progress),
        _ => Err(OversizedTransferError::Sequence(format!(
            "Invalid progress value (token: {token})"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{JsonRpcRequest, JsonRpcResponse};
    use crate::transport::oversized_transfer::codec::{
        build_oversized_frames, BuiltOversizedFrames, OversizedSenderOptions,
    };
    use crate::transport::oversized_transfer::frame::{CompletionMode, OversizedFrame};
    use crate::transport::oversized_transfer::START_PROGRESS;
    use serde_json::json;

    const TOKEN: &str = "tok";

    fn sample_response(id: i64, value: &str) -> JsonRpcMessage {
        JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: json!(id),
            result: json!({ "value": value }),
        })
    }

    fn build(message: &JsonRpcMessage, chunk_size: usize) -> BuiltOversizedFrames {
        let serialized = serde_json::to_string(message).unwrap();
        let opts = OversizedSenderOptions::new(TOKEN).with_chunk_size(chunk_size);
        build_oversized_frames(&serialized, &opts).unwrap()
    }

    fn start_frame(
        token: &str,
        progress: u64,
        digest: &str,
        total_bytes: u64,
        total_chunks: u64,
    ) -> JsonRpcNotification {
        OversizedFrame::Start {
            completion_mode: CompletionMode::Render,
            digest: digest.to_string(),
            total_bytes,
            total_chunks,
        }
        .into_progress_notification(token, progress, None)
        .unwrap()
    }

    fn chunk_frame(token: &str, progress: u64, data: &str) -> JsonRpcNotification {
        OversizedFrame::Chunk {
            data: data.to_string(),
        }
        .into_progress_notification(token, progress, None)
        .unwrap()
    }

    fn end_frame(token: &str, progress: u64) -> JsonRpcNotification {
        OversizedFrame::End
            .into_progress_notification(token, progress, None)
            .unwrap()
    }

    /// Drive a full ordered frame set through the receiver, returning the message.
    fn run_to_completion(
        receiver: &mut OversizedTransferReceiver,
        frames: BuiltOversizedFrames,
    ) -> JsonRpcMessage {
        let mut out = None;
        for frame in frames.into_ordered() {
            if let Some(message) = receiver.process_frame(&frame).unwrap() {
                out = Some(message);
            }
        }
        out.expect("transfer should complete on the end frame")
    }

    // ── roundtrip ───────────────────────────────────────────────────

    #[test]
    fn roundtrip_in_order() {
        let message = sample_response(1, &"x".repeat(60));
        let frames = build(&message, 8);
        assert!(frames.chunks.len() > 1);

        let mut receiver = OversizedTransferReceiver::new();
        let reconstructed = run_to_completion(&mut receiver, frames);

        assert_eq!(
            serde_json::to_value(&reconstructed).unwrap(),
            serde_json::to_value(&message).unwrap()
        );
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn roundtrip_request_message() {
        // Reassembly validates any JSON-RPC variant, not just responses.
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!("abc"),
            method: "tools/call".to_string(),
            params: Some(json!({ "blob": "y".repeat(40) })),
        });
        let frames = build(&message, 10);

        let mut receiver = OversizedTransferReceiver::new();
        let reconstructed = run_to_completion(&mut receiver, frames);
        assert_eq!(
            serde_json::to_value(&reconstructed).unwrap(),
            serde_json::to_value(&message).unwrap()
        );
    }

    #[test]
    fn roundtrip_multibyte_payload() {
        // Small chunks force boundaries inside multibyte UTF-8 runs.
        let message = sample_response(7, "héllo 🦀 wörld 日本語 ☃ même 🚀🚀");
        let frames = build(&message, 5);

        let mut receiver = OversizedTransferReceiver::new();
        let reconstructed = run_to_completion(&mut receiver, frames);
        assert_eq!(
            serde_json::to_value(&reconstructed).unwrap(),
            serde_json::to_value(&message).unwrap()
        );
    }

    #[test]
    fn roundtrip_out_of_order_within_window() {
        let message = sample_response(2, &"z".repeat(60));
        let frames = build(&message, 8);
        assert!(frames.chunks.len() >= 3);

        let mut receiver = OversizedTransferReceiver::with_policy(TransferPolicy {
            max_out_of_order_window: 4,
            max_out_of_order_chunks: 4,
            ..Default::default()
        });

        receiver.process_frame(&frames.start).unwrap();
        // Deliver the first two chunks swapped.
        receiver.process_frame(&frames.chunks[1]).unwrap();
        receiver.process_frame(&frames.chunks[0]).unwrap();
        for chunk in &frames.chunks[2..] {
            receiver.process_frame(chunk).unwrap();
        }
        let reconstructed = receiver.process_frame(&frames.end).unwrap().unwrap();

        assert_eq!(
            serde_json::to_value(&reconstructed).unwrap(),
            serde_json::to_value(&message).unwrap()
        );
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn roundtrip_accept_gated_handshake() {
        let message = sample_response(6, &"q".repeat(40));
        let serialized = serde_json::to_string(&message).unwrap();
        let opts = OversizedSenderOptions::new(TOKEN)
            .with_chunk_size(8)
            .with_accept_handshake(true);
        let frames = build_oversized_frames(&serialized, &opts).unwrap();

        let mut receiver = OversizedTransferReceiver::new();
        receiver.process_frame(&frames.start).unwrap();
        // Sender waits for the receiver's accept on the reserved slot 2.
        let accept = OversizedFrame::Accept
            .into_progress_notification(TOKEN, 2, None)
            .unwrap();
        receiver.process_frame(&accept).unwrap();
        for chunk in &frames.chunks {
            receiver.process_frame(chunk).unwrap();
        }
        let reconstructed = receiver.process_frame(&frames.end).unwrap().unwrap();

        assert_eq!(
            serde_json::to_value(&reconstructed).unwrap(),
            serde_json::to_value(&message).unwrap()
        );
    }

    // ── out-of-order policy ─────────────────────────────────────────

    #[test]
    fn out_of_order_gap_over_window_is_policy_error() {
        let message = sample_response(2, "abcdefghijklmnop");
        let frames = build(&message, 4);
        assert!(frames.chunks.len() >= 3);

        let mut receiver = OversizedTransferReceiver::with_policy(TransferPolicy {
            max_out_of_order_window: 1,
            ..Default::default()
        });
        receiver.process_frame(&frames.start).unwrap();

        // Jump straight to the third chunk: forward gap 2 > window 1.
        let err = receiver.process_frame(&frames.chunks[2]).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Policy(_)));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn chunk_with_u64_max_progress_does_not_panic() {
        // Regression: the forward-gap subtraction must not overflow when a
        // hostile peer sends progress = u64::MAX (previously panicked under i64).
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 4, 1))
            .unwrap();

        let err = receiver
            .process_frame(&chunk_frame(TOKEN, u64::MAX, "x"))
            .unwrap_err();
        // Rejected cleanly (huge forward gap → Policy) rather than panicking.
        assert!(matches!(
            err,
            OversizedTransferError::Policy(_) | OversizedTransferError::Sequence(_)
        ));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn first_chunk_skipping_reserved_accept_slot_is_sequence_error() {
        // 5 declared chunks keep slot 4 within the declared bounds (slots 2..=6),
        // so this exercises the reserved-slot guard, not the bounds check.
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 20, 5))
            .unwrap();

        // First chunk jumps to slot 4, skipping the reserved accept slot (2) and
        // the first chunk slot (3) while both are still empty.
        let err = receiver
            .process_frame(&chunk_frame(TOKEN, 4, "data"))
            .unwrap_err();
        assert!(matches!(err, OversizedTransferError::Sequence(_)));
        assert!(err.to_string().contains("reserved accept slot"));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn accept_not_advancing_past_start_is_sequence_error() {
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, START_PROGRESS, "sha256:abcd", 4, 1))
            .unwrap();

        // The accept frame MUST advance past the start slot; progress equal to
        // START_PROGRESS does not.
        let accept = OversizedFrame::Accept
            .into_progress_notification(TOKEN, START_PROGRESS, None)
            .unwrap();
        let err = receiver.process_frame(&accept).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Sequence(_)));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    /// An `accept` on the wrong progress slot is rejected by the receiver, so a
    /// transport that wakes the sender's accept-waiter only on a successful
    /// `process_frame` leaves the oneshot unfired — the sender stays blocked and
    /// falls back to its accept-timeout → abort (see [`super`]'s sibling sender
    /// test `missed_accept_returns_abort`).
    ///
    /// Scope: this pins the engine-level accept validation (`handle_accept`
    /// requires `progress > start_progress`). The live *client* upload path wakes
    /// its waiter on `cvm.frameType == "accept"` + a matching token alone (the
    /// accept's slot is only meaningful to a reassembling receiver), so this models
    /// the receiver/reassembly contract, not that path.
    #[test]
    fn invalid_accept_leaves_oneshot_unfired() {
        use tokio::sync::oneshot;

        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, START_PROGRESS, "sha256:abcd", 4, 1))
            .unwrap();

        // The sender registers a oneshot before publishing `start`; a transport
        // wakes it only once the receiver has accepted the accept frame.
        let (accept_tx, mut accept_rx) = oneshot::channel::<()>();

        // An accept on the start slot is invalid (it must advance past start).
        let invalid_accept = OversizedFrame::Accept
            .into_progress_notification(TOKEN, START_PROGRESS, None)
            .unwrap();

        // Fire-on-Ok: wake the waiter only if the receiver accepts the frame.
        match receiver.process_frame(&invalid_accept) {
            Ok(_) => {
                let _ = accept_tx.send(());
            }
            Err(error) => assert!(matches!(error, OversizedTransferError::Sequence(_))),
        }

        // The accept was rejected, so the oneshot was never fired: still `Empty`
        // (the sender — held alive here — has not been signalled).
        assert!(
            matches!(
                accept_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ),
            "an invalid accept must not fire the sender's accept-waiter"
        );
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    // ── integrity ───────────────────────────────────────────────────

    #[test]
    fn digest_mismatch_is_digest_error() {
        let message = sample_response(1, "hello");
        let serialized = serde_json::to_string(&message).unwrap();
        let total_bytes = serialized.len() as u64;

        let mut receiver = OversizedTransferReceiver::new();
        // Correct byte length, but a wrong (well-formed) digest.
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:deadbeef", total_bytes, 1))
            .unwrap();
        receiver
            .process_frame(&chunk_frame(TOKEN, 2, &serialized))
            .unwrap();
        let err = receiver.process_frame(&end_frame(TOKEN, 3)).unwrap_err();

        assert!(matches!(err, OversizedTransferError::Digest(_)));
        assert!(err.to_string().to_lowercase().contains("digest"));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn byte_length_mismatch_is_digest_error() {
        let message = sample_response(1, "hello");
        let serialized = serde_json::to_string(&message).unwrap();
        let digest = sha256_digest(&serialized);
        let wrong_total = serialized.len() as u64 + 1;

        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, &digest, wrong_total, 1))
            .unwrap();
        receiver
            .process_frame(&chunk_frame(TOKEN, 2, &serialized))
            .unwrap();
        let err = receiver.process_frame(&end_frame(TOKEN, 3)).unwrap_err();

        assert!(matches!(err, OversizedTransferError::Digest(_)));
        assert!(err.to_string().to_lowercase().contains("byte length"));
    }

    // ── sequencing ──────────────────────────────────────────────────

    #[test]
    fn duplicate_start_is_sequence_error() {
        let start = start_frame(TOKEN, 1, "sha256:abcd", 4, 1);
        let mut receiver = OversizedTransferReceiver::new();
        receiver.process_frame(&start).unwrap();

        let err = receiver.process_frame(&start).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Sequence(_)));
        assert!(err.to_string().contains("Duplicate start"));
        // The pre-existing transfer is left intact (duplicate is rejected, not reset).
        assert_eq!(receiver.active_transfer_count(), 1);
    }

    #[test]
    fn chunk_count_mismatch_is_reassembly_error() {
        let mut receiver = OversizedTransferReceiver::new();
        // Declares two chunks but only one arrives before end.
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 8, 2))
            .unwrap();
        receiver
            .process_frame(&chunk_frame(TOKEN, 2, "abcd"))
            .unwrap();
        let err = receiver.process_frame(&end_frame(TOKEN, 3)).unwrap_err();

        assert!(matches!(err, OversizedTransferError::Reassembly(_)));
        assert!(err.to_string().contains("Expected 2 chunks but received 1"));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn conflicting_duplicate_chunk_is_sequence_error() {
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 8, 2))
            .unwrap();
        receiver
            .process_frame(&chunk_frame(TOKEN, 2, "abcd"))
            .unwrap();
        let err = receiver
            .process_frame(&chunk_frame(TOKEN, 2, "wxyz"))
            .unwrap_err();

        assert!(matches!(err, OversizedTransferError::Sequence(_)));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn identical_duplicate_chunk_is_idempotent() {
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 8, 2))
            .unwrap();
        receiver
            .process_frame(&chunk_frame(TOKEN, 2, "abcd"))
            .unwrap();
        // Re-delivering the identical chunk is a no-op, not an error.
        assert!(receiver
            .process_frame(&chunk_frame(TOKEN, 2, "abcd"))
            .unwrap()
            .is_none());
        assert_eq!(receiver.active_transfer_count(), 1);
    }

    #[test]
    fn end_not_advancing_progress_is_sequence_error() {
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 4, 1))
            .unwrap();
        receiver
            .process_frame(&chunk_frame(TOKEN, 2, "test"))
            .unwrap();
        // End at the same progress as the last chunk must not advance.
        let err = receiver.process_frame(&end_frame(TOKEN, 2)).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Sequence(_)));
    }

    #[test]
    fn missing_token_is_sequence_error() {
        let frame = chunk_frame("", 2, "abcd");
        let mut receiver = OversizedTransferReceiver::new();
        let err = receiver.process_frame(&frame).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Sequence(_)));
        assert!(err.to_string().contains("missing progressToken"));
    }

    #[test]
    fn non_positive_progress_is_sequence_error() {
        let frame = start_frame(TOKEN, 0, "sha256:abcd", 4, 1);
        let mut receiver = OversizedTransferReceiver::new();
        let err = receiver.process_frame(&frame).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Sequence(_)));
    }

    // ── abort ───────────────────────────────────────────────────────

    #[test]
    fn abort_terminates_active_transfer() {
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 4, 1))
            .unwrap();

        let abort = OversizedFrame::Abort {
            reason: Some("peer cancelled".to_string()),
        }
        .into_progress_notification(TOKEN, 5, None)
        .unwrap();
        let err = receiver.process_frame(&abort).unwrap_err();

        assert!(err.is_abort());
        match err {
            OversizedTransferError::Abort { token, reason } => {
                assert_eq!(token, TOKEN);
                assert_eq!(reason.as_deref(), Some("peer cancelled"));
            }
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn orphan_abort_still_errors() {
        let mut receiver = OversizedTransferReceiver::new();
        let abort = OversizedFrame::Abort { reason: None }
            .into_progress_notification(TOKEN, 2, None)
            .unwrap();
        assert!(receiver.process_frame(&abort).unwrap_err().is_abort());
    }

    // ── admission limits ────────────────────────────────────────────

    #[test]
    fn start_over_byte_limit_is_policy_error() {
        let mut receiver = OversizedTransferReceiver::with_policy(TransferPolicy {
            max_transfer_bytes: 10,
            ..Default::default()
        });
        let err = receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 11, 1))
            .unwrap_err();
        assert!(matches!(err, OversizedTransferError::Policy(_)));
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn start_over_chunk_limit_is_policy_error() {
        let mut receiver = OversizedTransferReceiver::with_policy(TransferPolicy {
            max_transfer_chunks: 1,
            ..Default::default()
        });
        let err = receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 4, 2))
            .unwrap_err();
        assert!(matches!(err, OversizedTransferError::Policy(_)));
    }

    #[test]
    fn start_over_concurrency_limit_is_policy_error() {
        let mut receiver = OversizedTransferReceiver::with_policy(TransferPolicy {
            max_concurrent_transfers: 1,
            ..Default::default()
        });
        receiver
            .process_frame(&start_frame("tok-a", 1, "sha256:abcd", 4, 1))
            .unwrap();
        let err = receiver
            .process_frame(&start_frame("tok-b", 1, "sha256:efgh", 4, 1))
            .unwrap_err();
        assert!(matches!(err, OversizedTransferError::Policy(_)));
    }

    #[test]
    fn start_with_malformed_digest_is_reassembly_error() {
        let mut receiver = OversizedTransferReceiver::new();
        // Missing the "sha256:" prefix.
        let err = receiver
            .process_frame(&start_frame(TOKEN, 1, "abcd", 4, 1))
            .unwrap_err();
        assert!(matches!(err, OversizedTransferError::Reassembly(_)));
    }

    // ── orphan / non-frame handling ─────────────────────────────────

    #[test]
    fn orphan_late_frames_are_ignored() {
        let mut receiver = OversizedTransferReceiver::new();
        let accept = OversizedFrame::Accept
            .into_progress_notification("orphan-accept", 2, None)
            .unwrap();
        assert!(receiver.process_frame(&accept).unwrap().is_none());
        assert!(receiver
            .process_frame(&chunk_frame("orphan-chunk", 2, "x"))
            .unwrap()
            .is_none());
        assert!(receiver
            .process_frame(&end_frame("orphan-end", 2))
            .unwrap()
            .is_none());
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn non_frame_progress_notification_is_passthrough_none() {
        let mut receiver = OversizedTransferReceiver::new();
        let plain = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({ "progressToken": "t", "progress": 3 })),
        };
        assert!(receiver.process_frame(&plain).unwrap().is_none());

        let no_params = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/initialized".to_string(),
            params: None,
        };
        assert!(receiver.process_frame(&no_params).unwrap().is_none());
    }

    #[test]
    fn is_oversized_frame_detects_cvm_payload() {
        let frame = end_frame(TOKEN, 3);
        assert!(OversizedTransferReceiver::is_oversized_frame(&frame));

        let plain = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({ "progressToken": "t", "progress": 3 })),
        };
        assert!(!OversizedTransferReceiver::is_oversized_frame(&plain));
    }

    #[test]
    fn clear_releases_in_flight_state() {
        let mut receiver = OversizedTransferReceiver::new();
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 4, 1))
            .unwrap();
        assert_eq!(receiver.active_transfer_count(), 1);
        receiver.clear();
        assert_eq!(receiver.active_transfer_count(), 0);
    }

    #[test]
    fn reassembled_payload_exceeds_one_megabyte() {
        // The whole point of CEP-22: reassemble a payload larger than the 1 MB
        // per-event cap. Chunks stay small; the result is ~1.2 MB.
        let message = sample_response(1, &"m".repeat(1_200_000));
        let serialized = serde_json::to_string(&message).unwrap();
        assert!(serialized.len() > crate::core::constants::MAX_MESSAGE_SIZE);

        let frames = build(&message, 48_000);
        let mut receiver = OversizedTransferReceiver::new();
        let reconstructed = run_to_completion(&mut receiver, frames);
        assert_eq!(
            serde_json::to_value(&reconstructed).unwrap(),
            serde_json::to_value(&message).unwrap()
        );
    }

    // ── watchdog: admitted_at / remove_expired / is_tracking ────────────

    fn watchdog_policy(transfer_timeout_ms: u64) -> TransferPolicy {
        TransferPolicy {
            transfer_timeout_ms,
            ..TransferPolicy::default()
        }
    }

    /// Backdate a tracked transfer's admission so deadline checks are
    /// deterministic (no sleeping in unit tests).
    fn backdate_admission(receiver: &mut OversizedTransferReceiver, token: &str, ms: u64) {
        receiver
            .transfers
            .get_mut(token)
            .expect("transfer must be tracked")
            .admitted_at -= Duration::from_millis(ms);
    }

    #[test]
    fn remove_expired_reaps_past_deadline_and_orphans_late_frames() {
        let mut receiver = OversizedTransferReceiver::with_policy(watchdog_policy(50));
        let mut frames = build(&sample_response(1, "payload"), 4).into_ordered();
        let rest = frames.split_off(1);
        receiver.process_frame(&frames[0]).unwrap();
        assert!(receiver.is_tracking(TOKEN));

        backdate_admission(&mut receiver, TOKEN, 100);
        let reaped = receiver.remove_expired();
        assert_eq!(reaped, vec![TOKEN.to_string()]);
        assert!(!receiver.is_tracking(TOKEN));
        assert_eq!(receiver.active_transfer_count(), 0);

        // Late chunk/end frames of the reaped transfer are orphan-ignored:
        // no error, and nothing is ever surfaced.
        for frame in rest {
            assert!(receiver.process_frame(&frame).unwrap().is_none());
        }
    }

    #[test]
    fn remove_expired_skips_unexpired_transfers() {
        let mut receiver = OversizedTransferReceiver::with_policy(watchdog_policy(50));
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 4, 1))
            .unwrap();
        receiver
            .process_frame(&start_frame("tok-stale", 1, "sha256:abcd", 4, 1))
            .unwrap();

        backdate_admission(&mut receiver, "tok-stale", 100);
        let reaped = receiver.remove_expired();
        assert_eq!(reaped, vec!["tok-stale".to_string()]);
        assert!(receiver.is_tracking(TOKEN), "fresh transfer must survive");
        assert!(!receiver.is_tracking("tok-stale"));
    }

    #[test]
    fn remove_expired_zero_timeout_disables_watchdog() {
        // 0 means "no watchdog" (sweep skipped), NOT instant expiry.
        let mut receiver = OversizedTransferReceiver::with_policy(watchdog_policy(0));
        receiver
            .process_frame(&start_frame(TOKEN, 1, "sha256:abcd", 4, 1))
            .unwrap();
        backdate_admission(&mut receiver, TOKEN, 200);

        assert!(receiver.remove_expired().is_empty());
        assert!(receiver.is_tracking(TOKEN));
    }

    #[test]
    fn reaped_token_is_readmittable_as_fresh_transfer() {
        // remove_expired frees the slot; a later `start` re-using the token is
        // a fresh transfer (duplicate-start would error if state lingered), and
        // it runs to completion.
        let mut receiver = OversizedTransferReceiver::with_policy(watchdog_policy(50));
        let message = sample_response(7, "again");
        let mut first = build(&message, 4).into_ordered();
        first.truncate(2); // start + one chunk, then the sender stalls
        for frame in &first {
            receiver.process_frame(frame).unwrap();
        }

        backdate_admission(&mut receiver, TOKEN, 100);
        assert_eq!(receiver.remove_expired(), vec![TOKEN.to_string()]);

        let reconstructed = run_to_completion(&mut receiver, build(&message, 4));
        assert_eq!(
            serde_json::to_value(&reconstructed).unwrap(),
            serde_json::to_value(&message).unwrap()
        );
        assert!(
            !receiver.is_tracking(TOKEN),
            "completed transfer is released"
        );
    }

    #[test]
    fn is_tracking_reflects_transfer_lifecycle() {
        let mut receiver = OversizedTransferReceiver::new();
        assert!(!receiver.is_tracking(TOKEN));

        let frames = build(&sample_response(1, "lifecycle"), 4);
        let ordered = frames.into_ordered();
        let (end, mid) = ordered.split_last().expect("frames are never empty");

        for frame in mid {
            receiver.process_frame(frame).unwrap();
            assert!(receiver.is_tracking(TOKEN), "tracked while in flight");
        }
        receiver
            .process_frame(end)
            .unwrap()
            .expect("end frame completes the transfer");
        assert!(!receiver.is_tracking(TOKEN), "released after completion");
    }
}
