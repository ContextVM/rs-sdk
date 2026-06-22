//! Pure framing codec: digest, UTF-8 char-aware splitting, and frame building.
//!
//! Ports `sdk/src/transport/oversized-transfer/sender.ts`. No transport or I/O —
//! given a serialized JSON-RPC string this produces the ordered sequence of
//! `notifications/progress` frames a sender publishes.

use sha2::{Digest, Sha256};

use crate::core::types::JsonRpcNotification;

use super::constants::{ACCEPT_PROGRESS, DEFAULT_CHUNK_SIZE, DIGEST_PREFIX, START_PROGRESS};
use super::errors::OversizedTransferError;
use super::frame::{CompletionMode, OversizedFrame};

/// Options controlling how a payload is split into frames.
#[derive(Debug, Clone)]
pub struct OversizedSenderOptions {
    /// The MCP `progressToken` stamped on every frame in the transfer.
    pub progress_token: String,
    /// Per-chunk byte size. Defaults to [`DEFAULT_CHUNK_SIZE`].
    pub chunk_size_bytes: usize,
    /// Whether to reserve the `accept` slot (progress 2) for a handshake, so the
    /// first `chunk` starts at progress 3. When `false`, chunks start at 2.
    pub needs_accept_handshake: bool,
}

impl OversizedSenderOptions {
    /// Create options for `progress_token` with default chunk size and no handshake.
    pub fn new(progress_token: impl Into<String>) -> Self {
        Self {
            progress_token: progress_token.into(),
            chunk_size_bytes: DEFAULT_CHUNK_SIZE,
            needs_accept_handshake: false,
        }
    }

    /// Override the per-chunk byte size.
    pub fn with_chunk_size(mut self, chunk_size_bytes: usize) -> Self {
        self.chunk_size_bytes = chunk_size_bytes;
        self
    }

    /// Set whether the `accept` handshake slot is reserved.
    pub fn with_accept_handshake(mut self, needs_accept_handshake: bool) -> Self {
        self.needs_accept_handshake = needs_accept_handshake;
        self
    }
}

/// The ordered frames produced from a serialized payload.
#[derive(Debug, Clone)]
pub struct BuiltOversizedFrames {
    /// The opening `start` frame (progress [`START_PROGRESS`]).
    pub start: JsonRpcNotification,
    /// The `chunk` frames in ascending `progress` order.
    pub chunks: Vec<JsonRpcNotification>,
    /// The closing `end` frame.
    pub end: JsonRpcNotification,
}

impl BuiltOversizedFrames {
    /// Total number of frames (`start` + chunks + `end`).
    pub fn frame_count(&self) -> usize {
        self.chunks.len() + 2
    }

    /// Consume into a flat vector in canonical send order: `start`, chunks…, `end`.
    pub fn into_ordered(self) -> Vec<JsonRpcNotification> {
        let mut frames = Vec::with_capacity(self.frame_count());
        frames.push(self.start);
        frames.extend(self.chunks);
        frames.push(self.end);
        frames
    }
}

/// UTF-8 byte length of a string (Rust strings are already UTF-8).
pub fn utf8_byte_len(value: &str) -> usize {
    value.len()
}

/// Compute the CEP-22 digest of `value`: `"sha256:"` + lowercase hex of the
/// SHA-256 of its UTF-8 bytes.
pub fn sha256_digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{DIGEST_PREFIX}{}", hex::encode(hasher.finalize()))
}

/// Split `value` into fragments each at most `max_bytes` UTF-8 bytes, never
/// breaking a multibyte codepoint across a boundary.
///
/// Concatenating the fragments in order reproduces `value` exactly. Returns an
/// error if `max_bytes` is zero or a single character is larger than `max_bytes`.
pub fn split_string_by_byte_size(
    value: &str,
    max_bytes: usize,
) -> Result<Vec<String>, OversizedTransferError> {
    if max_bytes == 0 {
        return Err(OversizedTransferError::Policy(
            "chunk_size_bytes must be greater than zero".to_string(),
        ));
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_bytes = 0usize;

    for ch in value.chars() {
        let char_bytes = ch.len_utf8();
        if char_bytes > max_bytes {
            return Err(OversizedTransferError::Policy(format!(
                "Unable to split message: single character exceeds chunk size ({char_bytes} > {max_bytes})"
            )));
        }

        if current_bytes > 0 && current_bytes + char_bytes > max_bytes {
            chunks.push(std::mem::take(&mut current));
            current_bytes = 0;
        }

        current.push(ch);
        current_bytes += char_bytes;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    Ok(chunks)
}

/// Build the ordered `start` / `chunk…` / `end` frames for `serialized`.
///
/// Serializes once at the call site (the caller passes the exact JSON-RPC
/// string), then: computes the digest over that string, char-aware splits it,
/// and lays the chunks out on `progress` slots. When
/// [`needs_accept_handshake`](OversizedSenderOptions::needs_accept_handshake) is
/// set, progress 2 is reserved for the receiver's `accept` and chunks begin at 3.
pub fn build_oversized_frames(
    serialized: &str,
    options: &OversizedSenderOptions,
) -> crate::core::error::Result<BuiltOversizedFrames> {
    let total_bytes = utf8_byte_len(serialized) as u64;
    let digest = sha256_digest(serialized);

    let text_chunks = split_string_by_byte_size(serialized, options.chunk_size_bytes)?;
    let total_chunks = text_chunks.len() as u64;

    // No-handshake: chunks reuse the reserved accept slot (progress 2).
    // Handshake: accept occupies slot 2, so chunks begin at slot 3.
    let chunk_base_progress = if options.needs_accept_handshake {
        ACCEPT_PROGRESS + 1
    } else {
        ACCEPT_PROGRESS
    };

    let token = options.progress_token.as_str();

    let start = OversizedFrame::Start {
        completion_mode: CompletionMode::Render,
        digest,
        total_bytes,
        total_chunks,
    }
    .into_progress_notification(token, START_PROGRESS, Some("starting oversized transfer"))?;

    let mut chunks = Vec::with_capacity(text_chunks.len());
    for (i, data) in text_chunks.into_iter().enumerate() {
        let progress = chunk_base_progress + i as u64;
        let frame = OversizedFrame::Chunk { data };
        chunks.push(frame.into_progress_notification(token, progress, None)?);
    }

    let end = OversizedFrame::End.into_progress_notification(
        token,
        chunk_base_progress + total_chunks,
        Some("oversized transfer complete"),
    )?;

    Ok(BuiltOversizedFrames { start, chunks, end })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn frame_type(notification: &JsonRpcNotification) -> String {
        notification.params.as_ref().unwrap()["cvm"]["frameType"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn progress(notification: &JsonRpcNotification) -> u64 {
        notification.params.as_ref().unwrap()["progress"]
            .as_u64()
            .unwrap()
    }

    // ── digest ──────────────────────────────────────────────────────

    #[test]
    fn sha256_digest_known_vectors() {
        // Reference SHA-256 vectors with the CEP-22 "sha256:" prefix.
        assert_eq!(
            sha256_digest(""),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_digest("abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_digest_has_prefix_and_lowercase_hex() {
        let digest = sha256_digest("hello world");
        assert!(digest.starts_with("sha256:"));
        let hex_part = &digest["sha256:".len()..];
        assert_eq!(hex_part.len(), 64);
        assert!(hex_part
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn utf8_byte_len_counts_bytes_not_chars() {
        assert_eq!(utf8_byte_len("abc"), 3);
        assert_eq!(utf8_byte_len("é"), 2);
        assert_eq!(utf8_byte_len("🦀"), 4);
        assert_eq!(utf8_byte_len("a🦀"), 5);
    }

    // ── split_string_by_byte_size ───────────────────────────────────

    #[test]
    fn split_ascii_exact_and_remainder() {
        let chunks = split_string_by_byte_size("abcdefghij", 4).unwrap();
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
        assert_eq!(chunks.concat(), "abcdefghij");
    }

    #[test]
    fn split_empty_yields_no_chunks() {
        assert!(split_string_by_byte_size("", 4).unwrap().is_empty());
    }

    #[test]
    fn split_rejects_zero_max() {
        let err = split_string_by_byte_size("abc", 0).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Policy(_)));
    }

    #[test]
    fn split_multibyte_never_breaks_codepoints() {
        // Mixed 1/2/3/4-byte codepoints; chunk size 5 forces boundaries mid-string.
        let original = "aé🦀b日x☃yz🦀";
        let chunks = split_string_by_byte_size(original, 5).unwrap();

        // Concatenation in order reproduces the exact bytes.
        assert_eq!(chunks.concat(), original);
        // No chunk exceeds the byte budget, and every chunk is valid UTF-8
        // (guaranteed by `String`, i.e. no codepoint was split).
        for chunk in &chunks {
            assert!(utf8_byte_len(chunk) <= 5, "chunk {chunk:?} exceeds 5 bytes");
            assert!(!chunk.is_empty());
        }
    }

    #[test]
    fn split_packs_multibyte_up_to_budget() {
        // Each 🦀 is 4 bytes; with a budget of 8 two fit per chunk.
        assert_eq!(
            split_string_by_byte_size("🦀🦀🦀", 8).unwrap(),
            vec!["🦀🦀", "🦀"]
        );
    }

    #[test]
    fn split_rejects_single_char_larger_than_budget() {
        // 🦀 is 4 bytes; a 3-byte budget cannot hold it.
        let err = split_string_by_byte_size("🦀", 3).unwrap_err();
        assert!(matches!(err, OversizedTransferError::Policy(_)));
    }

    // ── build_oversized_frames ──────────────────────────────────────

    #[test]
    fn build_lays_out_progress_slots_without_handshake() {
        let opts = OversizedSenderOptions::new("tok").with_chunk_size(4);
        let frames = build_oversized_frames("hello world", &opts).unwrap();

        // "hello world" (11 bytes) / 4 → 3 chunks.
        assert_eq!(frames.chunks.len(), 3);
        assert_eq!(frames.frame_count(), 5);

        assert_eq!(progress(&frames.start), 1);
        assert_eq!(frame_type(&frames.start), "start");
        // No handshake: chunks reuse the reserved accept slot (start at 2).
        assert_eq!(progress(&frames.chunks[0]), 2);
        assert_eq!(progress(&frames.chunks[1]), 3);
        assert_eq!(progress(&frames.chunks[2]), 4);
        assert_eq!(frame_type(&frames.chunks[0]), "chunk");
        assert_eq!(progress(&frames.end), 5);
        assert_eq!(frame_type(&frames.end), "end");
    }

    #[test]
    fn build_reserves_accept_slot_with_handshake() {
        let opts = OversizedSenderOptions::new("tok")
            .with_chunk_size(4)
            .with_accept_handshake(true);
        let frames = build_oversized_frames("hello world", &opts).unwrap();

        assert_eq!(progress(&frames.start), 1);
        // Handshake: slot 2 reserved for accept, chunks begin at 3.
        assert_eq!(progress(&frames.chunks[0]), 3);
        assert_eq!(progress(&frames.chunks[2]), 5);
        assert_eq!(progress(&frames.end), 6);
    }

    #[test]
    fn build_start_frame_declares_digest_bytes_chunks() {
        let opts = OversizedSenderOptions::new("tok").with_chunk_size(4);
        let frames = build_oversized_frames("hello world", &opts).unwrap();
        let cvm = &frames.start.params.as_ref().unwrap()["cvm"];

        assert_eq!(cvm["type"].as_str(), Some("oversized-transfer"));
        assert_eq!(cvm["completionMode"], Value::String("render".to_string()));
        assert_eq!(cvm["digest"], Value::String(sha256_digest("hello world")));
        assert_eq!(cvm["totalBytes"].as_u64(), Some(11));
        assert_eq!(cvm["totalChunks"].as_u64(), Some(3));
    }

    #[test]
    fn build_propagates_single_char_over_budget_error() {
        let opts = OversizedSenderOptions::new("tok").with_chunk_size(3);
        let err = build_oversized_frames("🦀", &opts).unwrap_err();
        // Surfaced through the crate error as an oversized-transfer policy error.
        assert!(matches!(
            err,
            crate::core::error::Error::OversizedTransfer(OversizedTransferError::Policy(_))
        ));
    }

    #[test]
    fn build_into_ordered_is_start_chunks_end() {
        let opts = OversizedSenderOptions::new("tok").with_chunk_size(4);
        let frames = build_oversized_frames("hello world", &opts).unwrap();
        let ordered = frames.into_ordered();
        assert_eq!(ordered.len(), 5);
        assert_eq!(frame_type(&ordered[0]), "start");
        assert_eq!(frame_type(&ordered[4]), "end");
        // Progress is strictly increasing across the whole sequence.
        let progresses: Vec<u64> = ordered.iter().map(progress).collect();
        assert_eq!(progresses, vec![1, 2, 3, 4, 5]);
    }
}
