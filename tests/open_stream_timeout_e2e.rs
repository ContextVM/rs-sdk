//! CEP-41 keepalive timer e2e tests — the live client keepalive sweep driven
//! deterministically.
//!
//! The reader session clock is `std::time::Instant` (unaffected by tokio's
//! `start_paused`), so instead of advancing a paused clock we inject an explicit
//! future `now` into [`NostrClientTransport::run_open_stream_keepalive_sweep`] and
//! invoke it manually — no real sleeps for the timer logic. A greybox "server"
//! (a `BaseTransport` over the shared mock store) hand-publishes the inbound
//! frames the client reads.
//!
//! Declared in `Cargo.toml` with `required-features = ["rmcp", "test-utils"]`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use contextvm_sdk::core::constants::CTXVM_MESSAGES_KIND;
use contextvm_sdk::core::types::EncryptionMode;
use contextvm_sdk::relay::mock::MockRelayPool;
use contextvm_sdk::transport::base::BaseTransport;
use contextvm_sdk::transport::client::{NostrClientTransport, NostrClientTransportConfig};
use contextvm_sdk::transport::open_stream::{OpenStreamConfig, OpenStreamFrame, OpenStreamSession};
use contextvm_sdk::{JsonRpcMessage, RelayPoolTrait};
use futures::StreamExt;
use nostr_sdk::prelude::*;

const IDLE_MS: u64 = 40;
const PROBE_MS: u64 = 60;
// Wide enough that the ~30 ms "let the close land" settle (after which `t0` is
// captured) is negligible against the survive/abort sweep offsets below.
const GRACE_MS: u64 = 400;

// ── harness ────────────────────────────────────────────────────────────────

fn greybox_server_base(server_pool: &Arc<MockRelayPool>) -> BaseTransport {
    BaseTransport {
        relay_pool: Arc::clone(server_pool) as Arc<dyn RelayPoolTrait>,
        encryption_mode: EncryptionMode::Disabled,
        is_connected: true,
    }
}

/// Hand-publish one open-stream frame from the greybox server to the client.
async fn publish_frame(
    base: &BaseTransport,
    client: &PublicKey,
    progress: u64,
    frame: OpenStreamFrame,
) {
    let notification = frame
        .into_progress_notification("tok", progress, None)
        .expect("build frame");
    base.send_mcp_message(
        &JsonRpcMessage::Notification(notification),
        client,
        CTXVM_MESSAGES_KIND,
        BaseTransport::create_recipient_tags(client),
        Some(false),
        None,
    )
    .await
    .expect("publish frame");
}

/// Start a client whose reader sessions use the short keepalive timeouts above.
async fn start_keepalive_client(
    client_pool: MockRelayPool,
    server_pubkey: &PublicKey,
) -> NostrClientTransport {
    let mut client = NostrClientTransport::with_relay_pool(
        NostrClientTransportConfig::default()
            .with_relay_urls(vec!["wss://mock.relay".to_string()])
            .with_server_pubkey(server_pubkey.to_hex())
            .with_encryption_mode(EncryptionMode::Disabled)
            .with_open_stream(
                OpenStreamConfig::enabled()
                    .with_idle_timeout_ms(IDLE_MS)
                    .with_probe_timeout_ms(PROBE_MS)
                    .with_close_grace_period_ms(GRACE_MS),
            ),
        Arc::new(client_pool) as Arc<dyn RelayPoolTrait>,
    )
    .await
    .expect("client transport");
    let _rx = client.take_message_receiver().expect("client rx");
    client.start().await.expect("client start");
    tokio::time::sleep(Duration::from_millis(20)).await;
    client
}

/// Poll for the reader session to exist and have observed its `start`.
async fn wait_for_started_session(client: &NostrClientTransport) -> OpenStreamSession {
    for _ in 0..200 {
        if let Some(session) = client.get_open_stream_session("tok").await {
            if session.has_started() {
                return session;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("reader session for 'tok' never started");
}

/// Poll the shared store for a published frame whose `cvm.frameType == frame_type`,
/// returning its `cvm.nonce` (or "" when absent).
async fn wait_for_published_frame(pool: &MockRelayPool, frame_type: &str) -> String {
    for _ in 0..200 {
        for event in pool.stored_events().await {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&event.content) else {
                continue;
            };
            let cvm = v.get("params").and_then(|p| p.get("cvm"));
            if cvm.and_then(|c| c.get("type")).and_then(|t| t.as_str()) != Some("open-stream") {
                continue;
            }
            if cvm
                .and_then(|c| c.get("frameType"))
                .and_then(|t| t.as_str())
                == Some(frame_type)
            {
                return cvm
                    .and_then(|c| c.get("nonce"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no published {frame_type} frame observed");
}

fn start_frame() -> OpenStreamFrame {
    OpenStreamFrame::Start { content_type: None }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_elapses_ping_emitted_pong_resets_stream_survives() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut client = start_keepalive_client(client_pool, &server_pubkey).await;
    let base = greybox_server_base(&server_pool);

    publish_frame(&base, &client_pubkey, 1, start_frame()).await;
    let session = wait_for_started_session(&client).await;
    let t0 = Instant::now();

    // Idle threshold crossed → the client probes with a `ping`.
    client
        .run_open_stream_keepalive_sweep(t0 + Duration::from_millis(IDLE_MS + 5))
        .await;
    let nonce = wait_for_published_frame(&server_pool, "ping").await;
    assert_eq!(nonce, "tok:1", "ping nonce should be {{token}}:{{n}}");

    // A matching pong clears the probe; the stream must NOT abort.
    publish_frame(&base, &client_pubkey, 2, OpenStreamFrame::Pong { nonce }).await;
    // Give the inbound pong time to land.
    tokio::time::sleep(Duration::from_millis(30)).await;
    client
        .run_open_stream_keepalive_sweep(t0 + Duration::from_millis(IDLE_MS + PROBE_MS + 10))
        .await;
    assert!(
        session.is_active(),
        "a matching pong must keep the stream alive"
    );

    let _ = client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_plus_probe_elapse_no_pong_stream_aborts() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut client = start_keepalive_client(client_pool, &server_pubkey).await;
    let base = greybox_server_base(&server_pool);

    publish_frame(&base, &client_pubkey, 1, start_frame()).await;
    let mut session = wait_for_started_session(&client).await;
    let t0 = Instant::now();

    // Probe, then let the probe deadline pass with no pong → abort.
    client
        .run_open_stream_keepalive_sweep(t0 + Duration::from_millis(IDLE_MS + 5))
        .await;
    let _ = wait_for_published_frame(&server_pool, "ping").await;
    client
        .run_open_stream_keepalive_sweep(t0 + Duration::from_millis(IDLE_MS + PROBE_MS + 10))
        .await;

    match session.next().await {
        Some(Err(error)) => assert!(
            error.to_string().contains("Probe timeout"),
            "expected a probe-timeout abort, got: {error}"
        ),
        other => panic!("expected a terminal abort, got {other:?}"),
    }
    assert!(!session.is_active());

    let _ = client.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_with_missing_chunk_waits_grace_then_aborts() {
    let (client_pool, server_pool) = MockRelayPool::create_pair();
    let server_pubkey = server_pool.mock_public_key();
    let client_pubkey = client_pool.mock_public_key();
    let server_pool = Arc::new(server_pool);

    let mut client = start_keepalive_client(client_pool, &server_pubkey).await;
    let base = greybox_server_base(&server_pool);

    // start, then an out-of-order chunk (index 1 — leaves a gap at 0), then a
    // close: the gap is unresolved with a chunk buffered, so the grace timer arms.
    publish_frame(&base, &client_pubkey, 1, start_frame()).await;
    let mut session = wait_for_started_session(&client).await;
    publish_frame(
        &base,
        &client_pubkey,
        2,
        OpenStreamFrame::Chunk {
            chunk_index: 1,
            data: "late".to_string(),
        },
    )
    .await;
    publish_frame(
        &base,
        &client_pubkey,
        3,
        OpenStreamFrame::Close {
            last_chunk_index: None,
        },
    )
    .await;
    // Let the close land (arming the grace timer).
    tokio::time::sleep(Duration::from_millis(30)).await;
    let t0 = Instant::now();

    // Before the grace deadline: nothing. After it: abort. (`t0` is ~30 ms past
    // the close, so the deadline sits at ~`t0 + GRACE_MS - 30`.)
    client
        .run_open_stream_keepalive_sweep(t0 + Duration::from_millis(100))
        .await;
    assert!(session.is_active(), "must survive until the grace deadline");
    client
        .run_open_stream_keepalive_sweep(t0 + Duration::from_millis(GRACE_MS + 100))
        .await;

    match session.next().await {
        Some(Err(error)) => assert!(
            error.to_string().contains("Close grace period expired"),
            "expected a close-grace abort, got: {error}"
        ),
        other => panic!("expected a terminal abort, got {other:?}"),
    }

    let _ = client.close().await;
}
