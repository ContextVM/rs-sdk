//! Conformance tests: CEP-22 oversized-transfer wire format.
//!
//! A CEP-22 frame is an MCP `notifications/progress` JSON-RPC notification whose
//! `params.cvm` object carries the transfer envelope. These tests pin the exact
//! on-wire JSON — method, progress-slot layout, camelCase field names, the
//! `oversized-transfer` type tag, and the `sha256:` digest format — so the
//! serialization cannot drift from the spec and stays byte-compatible with the
//! TypeScript SDK, which emits the identical shape.
//!
//! Frames are serialized through `JsonRpcMessage` (the type both transports
//! publish, `#[serde(untagged)]`), so the asserted JSON is exactly what lands in
//! a kind-25910 event's `content`. No transport or I/O — pure serialization.

use serde_json::Value;

use contextvm_sdk::transport::oversized_transfer::{
    build_oversized_frames, sha256_digest, BuiltOversizedFrames, OversizedFrame,
    OversizedSenderOptions, ACCEPT_PROGRESS, START_PROGRESS,
};
use contextvm_sdk::{JsonRpcMessage, JsonRpcNotification};

const TOKEN: &str = "tok-1";

// ── helpers ───────────────────────────────────────────────────────────────────

/// Serialize a frame exactly as a transport publishes it. `JsonRpcMessage` is
/// `#[serde(untagged)]`, so this is the literal kind-25910 `content`.
fn wire(notif: &JsonRpcNotification) -> Value {
    serde_json::to_value(JsonRpcMessage::Notification(notif.clone()))
        .expect("frame must serialize to JSON")
}

/// The `params.cvm` object of a frame's wire form.
fn cvm(notif: &JsonRpcNotification) -> Value {
    wire(notif)["params"]["cvm"].clone()
}

/// The `params.progress` slot of a frame's wire form.
fn progress(notif: &JsonRpcNotification) -> Option<u64> {
    wire(notif)["params"]["progress"].as_u64()
}

/// The canonical 3-chunk transfer of `"hello world"` (11 bytes, chunk size 4).
fn three_chunk_transfer(handshake: bool) -> BuiltOversizedFrames {
    let opts = OversizedSenderOptions::new(TOKEN)
        .with_chunk_size(4)
        .with_accept_handshake(handshake);
    build_oversized_frames("hello world", &opts).expect("build frames")
}

/// A standalone `accept` frame on its reserved slot (the server emits this; the
/// codec's `build_oversized_frames` lays out start/chunk/end but never accept).
fn accept_frame() -> JsonRpcNotification {
    OversizedFrame::Accept
        .into_progress_notification(TOKEN, ACCEPT_PROGRESS, None)
        .expect("build accept frame")
}

/// A standalone `abort` frame.
fn abort_frame() -> JsonRpcNotification {
    OversizedFrame::Abort {
        reason: Some("peer cancelled".to_string()),
    }
    .into_progress_notification(TOKEN, 9, None)
    .expect("build abort frame")
}

// ── frame envelope ──────────────────────────────────────────────────────────

#[test]
fn frame_is_a_notifications_progress_notification() {
    let frames = three_chunk_transfer(false);
    let w = wire(&frames.start);

    assert_eq!(
        w["jsonrpc"],
        Value::String("2.0".to_string()),
        "frame must be JSON-RPC 2.0"
    );
    assert_eq!(
        w["method"],
        Value::String("notifications/progress".to_string()),
        "frame method must be notifications/progress"
    );

    let params = &w["params"];
    assert!(params.is_object(), "params must be an object");
    assert_eq!(
        params["progressToken"],
        Value::String(TOKEN.to_string()),
        "params must carry the progressToken"
    );
    assert!(
        params["progress"].is_number(),
        "params.progress must be a number"
    );
    assert!(params["cvm"].is_object(), "params.cvm must be an object");
}

// ── cvm.type ──────────────────────────────────────────────────────────────────

#[test]
fn every_frame_carries_cvm_type_oversized_transfer() {
    let frames = three_chunk_transfer(false);
    let accept = accept_frame();
    let abort = abort_frame();

    for notif in [
        &frames.start,
        &frames.chunks[0],
        &frames.end,
        &accept,
        &abort,
    ] {
        assert_eq!(
            cvm(notif)["type"],
            Value::String("oversized-transfer".to_string()),
            "every cvm object must carry type=oversized-transfer"
        );
    }
}

// ── cvm.frameType ─────────────────────────────────────────────────────────────

#[test]
fn cvm_frame_type_discriminates_each_variant_in_lowercase() {
    let frames = three_chunk_transfer(false);

    assert_eq!(
        cvm(&frames.start)["frameType"],
        Value::String("start".into())
    );
    assert_eq!(
        cvm(&accept_frame())["frameType"],
        Value::String("accept".into())
    );
    assert_eq!(
        cvm(&frames.chunks[0])["frameType"],
        Value::String("chunk".into())
    );
    assert_eq!(cvm(&frames.end)["frameType"], Value::String("end".into()));
    assert_eq!(
        cvm(&abort_frame())["frameType"],
        Value::String("abort".into())
    );

    // The discriminator key is camelCase `frameType`, never snake_case.
    let start_cvm = cvm(&frames.start);
    let obj = start_cvm.as_object().unwrap();
    assert!(
        obj.contains_key("frameType"),
        "must use camelCase frameType"
    );
    assert!(
        !obj.contains_key("frame_type"),
        "snake_case frame_type must not appear on the wire"
    );
}

// ── start frame fields ────────────────────────────────────────────────────────

#[test]
fn start_frame_fields_are_camelcase_and_correctly_typed() {
    // "hello world" is 11 bytes; chunk size 4 → 3 chunks.
    let c = cvm(&three_chunk_transfer(false).start);

    assert_eq!(
        c["completionMode"],
        Value::String("render".to_string()),
        "v1 completion mode must be render"
    );
    assert_eq!(
        c["totalBytes"].as_u64(),
        Some(11),
        "totalBytes must equal the payload's UTF-8 byte length"
    );
    assert_eq!(
        c["totalChunks"].as_u64(),
        Some(3),
        "totalChunks must equal the chunk count"
    );
    assert!(
        c["digest"].as_str().unwrap().starts_with("sha256:"),
        "digest must carry the sha256: prefix"
    );

    // Field names are camelCase, never snake_case.
    let obj = c.as_object().unwrap();
    for camel in ["completionMode", "digest", "totalBytes", "totalChunks"] {
        assert!(
            obj.contains_key(camel),
            "start cvm missing camelCase key {camel}"
        );
    }
    for snake in ["completion_mode", "total_bytes", "total_chunks"] {
        assert!(
            !obj.contains_key(snake),
            "snake_case key {snake} must not appear on the wire"
        );
    }
}

// ── chunk frame fields ────────────────────────────────────────────────────────

#[test]
fn chunk_frames_carry_data_that_reconstructs_the_payload() {
    let frames = three_chunk_transfer(false);

    let mut reassembled = String::new();
    for chunk in &frames.chunks {
        let data = cvm(chunk)["data"]
            .as_str()
            .expect("chunk cvm.data must be a string")
            .to_string();
        reassembled.push_str(&data);
    }
    assert_eq!(
        reassembled, "hello world",
        "concatenated chunk data must reproduce the exact payload"
    );
}

// ── progress slots ────────────────────────────────────────────────────────────

#[test]
fn canonical_progress_slots_match_spec() {
    assert_eq!(START_PROGRESS, 1, "start frame occupies progress slot 1");
    assert_eq!(ACCEPT_PROGRESS, 2, "accept frame occupies progress slot 2");
}

#[test]
fn progress_slots_without_handshake() {
    // start=1; with no accept, chunks reuse the reserved slot 2; end = start + N + 1.
    let frames = three_chunk_transfer(false);
    assert_eq!(frames.chunks.len(), 3);

    assert_eq!(progress(&frames.start), Some(1));
    assert_eq!(progress(&frames.chunks[0]), Some(2));
    assert_eq!(progress(&frames.chunks[1]), Some(3));
    assert_eq!(progress(&frames.chunks[2]), Some(4));
    assert_eq!(
        progress(&frames.end),
        Some(5),
        "end = start(1) + chunks(3) + 1"
    );
}

#[test]
fn progress_slots_with_handshake() {
    // start=1; accept reserved at slot 2; first chunk shifts to 3; end follows.
    let frames = three_chunk_transfer(true);
    assert_eq!(frames.chunks.len(), 3);

    assert_eq!(progress(&frames.start), Some(1));
    assert_eq!(progress(&accept_frame()), Some(2));
    assert_eq!(progress(&frames.chunks[0]), Some(3));
    assert_eq!(progress(&frames.chunks[1]), Some(4));
    assert_eq!(progress(&frames.chunks[2]), Some(5));
    assert_eq!(progress(&frames.end), Some(6), "end follows the last chunk");
}

// ── digest format ─────────────────────────────────────────────────────────────

#[test]
fn digest_is_sha256_prefix_plus_lowercase_hex() {
    let digest = cvm(&three_chunk_transfer(false).start)["digest"]
        .as_str()
        .unwrap()
        .to_string();

    assert!(digest.starts_with("sha256:"), "must start with sha256:");
    let hex = &digest["sha256:".len()..];
    assert_eq!(hex.len(), 64, "SHA-256 hex must be 64 characters");
    assert!(
        hex.chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()),
        "digest hex must be lowercase"
    );
}

#[test]
fn digest_is_over_the_full_payload_not_per_chunk() {
    // Known-answer vector: SHA-256("abc"). Even fragmented into 3 single-byte
    // chunks, the start digest is the hash of the WHOLE payload.
    let abc = build_oversized_frames(
        "abc",
        &OversizedSenderOptions::new(TOKEN).with_chunk_size(1),
    )
    .expect("build frames");
    assert_eq!(
        abc.chunks.len(),
        3,
        "chunk size 1 must yield 3 chunks for abc"
    );
    assert_eq!(
        cvm(&abc.start)["digest"].as_str(),
        Some("sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "digest must be SHA-256 of the full payload \"abc\", independent of chunking"
    );

    // For an arbitrary multi-chunk payload, the start digest equals the hash of
    // the payload reconstructed from the chunk data (i.e. the full message).
    let payload = "the quick brown fox jumps";
    let frames = build_oversized_frames(
        payload,
        &OversizedSenderOptions::new(TOKEN).with_chunk_size(5),
    )
    .expect("build frames");
    let reconstructed: String = frames
        .chunks
        .iter()
        .map(|c| cvm(c)["data"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        reconstructed, payload,
        "chunks must reconstruct the payload"
    );
    assert_eq!(
        cvm(&frames.start)["digest"].as_str(),
        Some(sha256_digest(payload).as_str()),
        "digest must hash the full payload"
    );
}
