//! Per-peer registry of active CEP-41 reader sessions, keyed by `progressToken`.
//!
//! Ports `sdk/src/transport/open-stream/registry.ts`. Enforces admission
//! (max-concurrent → `Policy`; duplicate token → `Sequence`; a session is
//! created **only** on a `start` frame — any other first frame is a `Sequence`
//! error), routes inbound frames to the matching [`OpenStreamSession`], and
//! removes a session on its terminal close/abort (running optional per-session
//! hooks, which are always cleaned up even if they error).

use std::collections::HashMap;
use std::time::Instant;

use futures::future::BoxFuture;
use serde_json::Value;

use crate::core::types::JsonRpcNotification;
use crate::transport::oversized_transfer::progress_token_string;

use super::constants::{
    DEFAULT_MAX_BUFFERED_BYTES_PER_STREAM, DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM,
    DEFAULT_MAX_CONCURRENT_OPEN_STREAMS, DEFAULT_OPEN_STREAM_CLOSE_GRACE_PERIOD_MS,
    DEFAULT_OPEN_STREAM_IDLE_TIMEOUT_MS, DEFAULT_OPEN_STREAM_PROBE_TIMEOUT_MS,
};
use super::errors::OpenStreamError;
use super::frame::OpenStreamFrame;
use super::session::{
    FrameOutcome, KeepaliveAction, OpenStreamSession, OpenStreamSessionOptions, PublishFrame,
};

const LOG_TARGET: &str = "contextvm_sdk::transport::open_stream";

/// Hook fired after a session closes gracefully. Errors are logged and swallowed.
pub type RegistryCloseHook = Box<dyn FnOnce() -> BoxFuture<'static, crate::Result<()>> + Send>;

/// Hook fired after a session aborts (with the advisory reason). Errors are
/// logged and swallowed.
pub type RegistryAbortHook =
    Box<dyn FnOnce(Option<String>) -> BoxFuture<'static, crate::Result<()>> + Send>;

/// Reader admission / buffering / keepalive policy for a registry's sessions.
///
/// Projected from
/// [`OpenStreamConfig`](crate::transport::open_stream::OpenStreamConfig) via
/// `From<&OpenStreamConfig>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenStreamRegistryPolicy {
    /// Maximum concurrently active streams.
    pub max_concurrent_streams: usize,
    /// Maximum buffered + queued chunks per stream.
    pub max_buffered_chunks_per_stream: usize,
    /// Maximum buffered + queued payload bytes per stream.
    pub max_buffered_bytes_per_stream: usize,
    /// Idle interval before a reader probes with a `ping` (ms).
    pub idle_timeout_ms: u64,
    /// Time a reader waits for a `pong` before aborting (ms).
    pub probe_timeout_ms: u64,
    /// Grace period after a `close` with unresolved gaps before aborting (ms).
    pub close_grace_period_ms: u64,
}

impl Default for OpenStreamRegistryPolicy {
    fn default() -> Self {
        Self {
            max_concurrent_streams: DEFAULT_MAX_CONCURRENT_OPEN_STREAMS,
            max_buffered_chunks_per_stream: DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM,
            max_buffered_bytes_per_stream: DEFAULT_MAX_BUFFERED_BYTES_PER_STREAM,
            idle_timeout_ms: DEFAULT_OPEN_STREAM_IDLE_TIMEOUT_MS,
            probe_timeout_ms: DEFAULT_OPEN_STREAM_PROBE_TIMEOUT_MS,
            close_grace_period_ms: DEFAULT_OPEN_STREAM_CLOSE_GRACE_PERIOD_MS,
        }
    }
}

/// Optional per-session wiring supplied at creation time.
#[derive(Default)]
pub struct OpenStreamSessionInit {
    /// Outbound publisher for the reader's consumer `abort` frame.
    pub publish_frame: Option<PublishFrame>,
    /// Hook fired after a graceful close.
    pub on_close: Option<RegistryCloseHook>,
    /// Hook fired after an abort.
    pub on_abort: Option<RegistryAbortHook>,
}

/// A registered session plus its terminal lifecycle hooks.
struct RegistryEntry {
    session: OpenStreamSession,
    on_close: Option<RegistryCloseHook>,
    on_abort: Option<RegistryAbortHook>,
}

/// Registry of active CEP-41 reader sessions keyed by `progressToken`.
pub struct OpenStreamRegistry {
    policy: OpenStreamRegistryPolicy,
    sessions: HashMap<String, RegistryEntry>,
}

impl Default for OpenStreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenStreamRegistry {
    /// Create a registry with the default policy.
    pub fn new() -> Self {
        Self::with_policy(OpenStreamRegistryPolicy::default())
    }

    /// Create a registry with an explicit policy.
    pub fn with_policy(policy: OpenStreamRegistryPolicy) -> Self {
        Self {
            policy,
            sessions: HashMap::new(),
        }
    }

    /// Number of active sessions.
    pub fn size(&self) -> usize {
        self.sessions.len()
    }

    /// Look up an active session by token.
    pub fn get_session(&self, progress_token: &str) -> Option<OpenStreamSession> {
        self.sessions
            .get(progress_token)
            .map(|entry| entry.session.clone())
    }

    /// Returns `true` when `notification` carries a CEP-41 frame in `params.cvm`.
    ///
    /// Mirrors the TS `OpenStreamRegistry.isOpenStreamProgress` narrowing.
    pub fn is_open_stream_progress(notification: &JsonRpcNotification) -> bool {
        notification
            .params
            .as_ref()
            .and_then(|params| params.get("cvm"))
            .map(OpenStreamFrame::is_frame_value)
            .unwrap_or(false)
    }

    /// Create a session for `progress_token` with default wiring.
    pub fn create_session(
        &mut self,
        progress_token: impl Into<String>,
    ) -> Result<OpenStreamSession, OpenStreamError> {
        self.create_session_with(progress_token, OpenStreamSessionInit::default())
    }

    /// Create a session for `progress_token` with explicit wiring, enforcing the
    /// duplicate-token (`Sequence`) and max-concurrent (`Policy`) admission rules.
    pub fn create_session_with(
        &mut self,
        progress_token: impl Into<String>,
        init: OpenStreamSessionInit,
    ) -> Result<OpenStreamSession, OpenStreamError> {
        let progress_token = progress_token.into();
        if self.sessions.contains_key(&progress_token) {
            return Err(OpenStreamError::Sequence(format!(
                "Stream session already exists for {progress_token}"
            )));
        }
        if self.sessions.len() >= self.policy.max_concurrent_streams {
            return Err(OpenStreamError::Policy(
                "Maximum concurrent open streams exceeded".to_string(),
            ));
        }

        let session = OpenStreamSession::new(OpenStreamSessionOptions {
            progress_token: progress_token.clone(),
            max_buffered_chunks: self.policy.max_buffered_chunks_per_stream,
            max_buffered_bytes: self.policy.max_buffered_bytes_per_stream as u64,
            idle_timeout_ms: self.policy.idle_timeout_ms,
            probe_timeout_ms: self.policy.probe_timeout_ms,
            close_grace_period_ms: self.policy.close_grace_period_ms,
            publish_frame: init.publish_frame,
        });
        self.sessions.insert(
            progress_token,
            RegistryEntry {
                session: session.clone(),
                on_close: init.on_close,
                on_abort: init.on_abort,
            },
        );
        Ok(session)
    }

    /// Return the existing session for `progress_token`, creating one (with
    /// default wiring) if absent.
    pub fn get_or_create_session(
        &mut self,
        progress_token: impl Into<String>,
    ) -> Result<OpenStreamSession, OpenStreamError> {
        let progress_token = progress_token.into();
        if let Some(session) = self.get_session(&progress_token) {
            return Ok(session);
        }
        self.create_session(progress_token)
    }

    /// Route one inbound frame to its session, creating the session on a `start`.
    ///
    /// On a sequencing/policy violation the offending session is failed and
    /// removed before the error is returned; on a terminal close/abort the
    /// session is removed and its hook run (errors logged, never propagated).
    pub async fn process_frame(
        &mut self,
        now: Instant,
        notification: &JsonRpcNotification,
    ) -> Result<FrameOutcome, OpenStreamError> {
        let (progress_token, progress, frame) = parse_frame(notification)?;

        if !self.sessions.contains_key(&progress_token) {
            if frame.frame_type() != "start" {
                return Err(OpenStreamError::Sequence(format!(
                    "Received {} frame before start for {progress_token}",
                    frame.frame_type()
                )));
            }
            self.create_session_with(progress_token.clone(), OpenStreamSessionInit::default())?;
        }

        // Clone the Arc-backed handle out so the map is not borrowed across the
        // hook `.await`s below.
        let session = self
            .sessions
            .get(&progress_token)
            .expect("session present after create")
            .session
            .clone();

        match session.process_frame(now, progress, frame) {
            Ok(FrameOutcome::Closed) => {
                self.run_close(&progress_token).await;
                Ok(FrameOutcome::Closed)
            }
            Ok(FrameOutcome::Aborted(reason)) => {
                self.run_abort(&progress_token, reason.clone()).await;
                Ok(FrameOutcome::Aborted(reason))
            }
            Ok(other) => Ok(other),
            Err(error) => {
                session.fail(error.clone());
                // Forward the failure reason to the `on_abort` hook (TS passes
                // `onAbort(error.message)`); the inbound-abort path already forwards
                // the peer's reason — this covers the local processing-failure
                // branch so deferral/metrics hooks see why.
                self.run_abort(&progress_token, Some(error.to_string()))
                    .await;
                Err(error)
            }
        }
    }

    /// Drive the pure keepalive [`tick`](OpenStreamSession::tick) for every active
    /// session, returning the `(progress_token, action)` pairs that need an
    /// outbound send (`SendPing`) or signal a local abort (`Abort`). Sessions that
    /// aborted on this tick are removed (their slot is freed); the transport sweep
    /// performs the actual publish for each returned action.
    pub fn tick_all(&mut self, now: Instant) -> Vec<(String, KeepaliveAction)> {
        let mut actions = Vec::new();
        let mut aborted = Vec::new();
        for (token, entry) in self.sessions.iter() {
            match entry.session.tick(now) {
                KeepaliveAction::None => {}
                action => {
                    if matches!(action, KeepaliveAction::Abort(_)) {
                        aborted.push(token.clone());
                    }
                    actions.push((token.clone(), action));
                }
            }
        }
        for token in aborted {
            self.sessions.remove(&token);
        }
        actions
    }

    /// Dispose every session gracefully and drop them (runs no hooks).
    pub fn clear(&mut self) {
        for (_, entry) in self.sessions.drain() {
            entry.session.dispose();
        }
    }

    /// Consumer-cancel cleanup: finalize the session locally, run its `on_abort`
    /// hook, and **remove the entry** so the concurrency slot is freed.
    ///
    /// The session's `process_frame`/`tick` paths only remove an entry on an
    /// *inbound* terminal frame; a consumer that cancels its own read
    /// ([`OpenStreamSession::abort`]) finalizes + publishes an `abort` frame but
    /// leaves the registry entry counting against `max_concurrent_streams`. The
    /// transport calls this to close that gap when wiring cancel. The outbound
    /// `abort` *frame* is published by the caller via
    /// [`OpenStreamSession::abort`]; here `fail` only guarantees the local stream
    /// is terminal (idempotent if already finalized).
    pub async fn consumer_abort(&mut self, progress_token: &str, reason: Option<String>) {
        if let Some(entry) = self.sessions.remove(progress_token) {
            entry
                .session
                .fail(OpenStreamError::abort(progress_token, reason.clone()));
            if let Some(hook) = entry.on_abort {
                if let Err(error) = hook(reason).await {
                    tracing::debug!(
                        target: LOG_TARGET,
                        token = %progress_token,
                        %error,
                        "open-stream on_abort hook errored during consumer abort"
                    );
                }
            }
        }
    }

    /// Remove a closed session and run its `on_close` hook (errors swallowed).
    async fn run_close(&mut self, progress_token: &str) {
        if let Some(entry) = self.sessions.remove(progress_token) {
            if let Some(hook) = entry.on_close {
                if let Err(error) = hook().await {
                    tracing::debug!(
                        target: LOG_TARGET,
                        token = %progress_token,
                        %error,
                        "open-stream on_close hook errored"
                    );
                }
            }
        }
    }

    /// Remove an aborted session and run its `on_abort` hook (errors swallowed).
    async fn run_abort(&mut self, progress_token: &str, reason: Option<String>) {
        if let Some(entry) = self.sessions.remove(progress_token) {
            if let Some(hook) = entry.on_abort {
                if let Err(error) = hook(reason).await {
                    tracing::debug!(
                        target: LOG_TARGET,
                        token = %progress_token,
                        %error,
                        "open-stream on_abort hook errored"
                    );
                }
            }
        }
    }
}

/// Extract `(progressToken, progress, frame)` from a `notifications/progress`
/// payload, rejecting any non-CEP-41 or malformed notification with `Sequence`.
fn parse_frame(
    notification: &JsonRpcNotification,
) -> Result<(String, i64, OpenStreamFrame), OpenStreamError> {
    let params = notification.params.as_ref().ok_or_else(|| {
        OpenStreamError::Sequence("Open stream frame is missing params".to_string())
    })?;
    let frame = params
        .get("cvm")
        .and_then(OpenStreamFrame::from_cvm_value)
        .ok_or_else(|| {
            OpenStreamError::Sequence("Notification is not an open-stream frame".to_string())
        })?;

    let progress_token = params
        .get("progressToken")
        .and_then(progress_token_string)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            OpenStreamError::Sequence("Open stream frame is missing progressToken".to_string())
        })?;

    let progress = parse_progress(params.get("progress"), &progress_token)?;
    Ok((progress_token, progress, frame))
}

/// Parse the outer `progress` as an integer (CEP-41 requires it on every frame).
fn parse_progress(value: Option<&Value>, token: &str) -> Result<i64, OpenStreamError> {
    let progress = match value {
        Some(Value::Number(n)) => n.as_i64().or_else(|| {
            n.as_f64()
                .and_then(|f| if f.is_finite() { Some(f as i64) } else { None })
        }),
        _ => None,
    };
    progress.ok_or_else(|| {
        OpenStreamError::Sequence(format!("Invalid progress value (token: {token})"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use futures::StreamExt;
    use serde_json::json;

    fn now() -> Instant {
        Instant::now()
    }

    fn notif(token: &str, progress: u64, frame: OpenStreamFrame) -> JsonRpcNotification {
        frame
            .into_progress_notification(token, progress, None)
            .unwrap()
    }

    fn start() -> OpenStreamFrame {
        OpenStreamFrame::Start { content_type: None }
    }

    fn chunk(index: u64, data: &str) -> OpenStreamFrame {
        OpenStreamFrame::Chunk {
            chunk_index: index,
            data: data.to_string(),
        }
    }

    fn small_policy(max_concurrent: usize) -> OpenStreamRegistryPolicy {
        OpenStreamRegistryPolicy {
            max_concurrent_streams: max_concurrent,
            max_buffered_chunks_per_stream: 4,
            max_buffered_bytes_per_stream: 128,
            ..OpenStreamRegistryPolicy::default()
        }
    }

    #[tokio::test]
    async fn enforces_max_concurrent_and_reuses_slot_after_close() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(1));
        let _first = registry.create_session("token-1").unwrap();
        assert!(matches!(
            registry.create_session("token-2").unwrap_err(),
            OpenStreamError::Policy(_)
        ));

        // Drive token-1 to a graceful close through the registry to free the slot.
        registry
            .process_frame(now(), &notif("token-1", 1, start()))
            .await
            .unwrap();
        registry
            .process_frame(
                now(),
                &notif(
                    "token-1",
                    2,
                    OpenStreamFrame::Close {
                        last_chunk_index: None,
                    },
                ),
            )
            .await
            .unwrap();

        registry.create_session("token-2").unwrap();
        assert_eq!(registry.size(), 1);
    }

    #[test]
    fn get_or_create_reuses_the_same_session() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(2));
        let first = registry.get_or_create_session("token-shared").unwrap();
        let second = registry.get_or_create_session("token-shared").unwrap();
        assert!(first.shares_state_with(&second));
        assert_eq!(registry.size(), 1);
    }

    #[test]
    fn rejects_creating_a_duplicate_token() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(4));
        registry.create_session("token-dup").unwrap();
        // A second `create` for a live token is a sequence violation (not a new
        // slot and not silent reuse) — distinct from the concurrency `Policy` cap.
        assert!(matches!(
            registry.create_session("token-dup").unwrap_err(),
            OpenStreamError::Sequence(_)
        ));
        assert_eq!(registry.size(), 1);
    }

    #[tokio::test]
    async fn routes_a_duplicate_start_frame_to_a_sequence_error() {
        // Admission creates the session on the first `start`; a second `start`
        // routes to the live session, which rejects it (the `Duplicate start`
        // guard), and the registry then fails + removes the session.
        let mut registry = OpenStreamRegistry::with_policy(small_policy(2));
        registry
            .process_frame(now(), &notif("token-dup-start", 1, start()))
            .await
            .unwrap();
        let err = registry
            .process_frame(now(), &notif("token-dup-start", 2, start()))
            .await
            .unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
        assert!(registry.get_session("token-dup-start").is_none());
    }

    #[tokio::test]
    async fn rejects_non_start_frames_for_unknown_tokens() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(2));
        let err = registry
            .process_frame(now(), &notif("token-missing-start", 1, chunk(0, "orphan")))
            .await
            .unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
        assert!(registry.get_session("token-missing-start").is_none());
        assert_eq!(registry.size(), 0);
    }

    #[tokio::test]
    async fn terminates_session_when_frame_processing_fails_after_creation() {
        let mut registry = OpenStreamRegistry::with_policy(OpenStreamRegistryPolicy {
            max_buffered_bytes_per_stream: 4,
            ..small_policy(2)
        });
        registry
            .process_frame(now(), &notif("token-fail", 1, start()))
            .await
            .unwrap();
        let mut session = registry.get_session("token-fail").unwrap();

        // 'hello' (5 bytes) at an out-of-order index exceeds the 4-byte budget.
        let err = registry
            .process_frame(now(), &notif("token-fail", 2, chunk(1, "hello")))
            .await
            .unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
        assert!(registry.get_session("token-fail").is_none());

        // The session was failed: its stream surfaces the same error class.
        match session.next().await {
            Some(Err(OpenStreamError::Sequence(_))) => {}
            other => panic!("expected sequence error on the stream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn applies_default_buffering_limits() {
        let mut registry = OpenStreamRegistry::new();
        registry
            .process_frame(now(), &notif("token-chunks", 1, start()))
            .await
            .unwrap();
        // Buffer exactly DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM out-of-order chunks.
        for i in 0..DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM as u64 {
            registry
                .process_frame(now(), &notif("token-chunks", i + 2, chunk(i + 1, "x")))
                .await
                .unwrap();
        }
        let err = registry
            .process_frame(
                now(),
                &notif(
                    "token-chunks",
                    DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM as u64 + 2,
                    chunk(DEFAULT_MAX_BUFFERED_CHUNKS_PER_STREAM as u64 + 1, "x"),
                ),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));

        // The default byte budget is enforced too.
        let mut byte_registry = OpenStreamRegistry::new();
        byte_registry
            .process_frame(now(), &notif("token-bytes", 1, start()))
            .await
            .unwrap();
        let oversized = "x".repeat(DEFAULT_MAX_BUFFERED_BYTES_PER_STREAM + 1);
        let err = byte_registry
            .process_frame(now(), &notif("token-bytes", 2, chunk(1, &oversized)))
            .await
            .unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn ping_frame_routes_to_a_pong_outcome() {
        let mut registry = OpenStreamRegistry::new();
        registry
            .process_frame(now(), &notif("token-timers", 1, start()))
            .await
            .unwrap();
        let outcome = registry
            .process_frame(
                now(),
                &notif(
                    "token-timers",
                    2,
                    OpenStreamFrame::Ping {
                        nonce: "peer-nonce".to_string(),
                    },
                ),
            )
            .await
            .unwrap();
        assert_eq!(outcome, FrameOutcome::SendPong("peer-nonce".to_string()));
    }

    #[tokio::test]
    async fn clear_disposes_active_sessions() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(2));
        let session = registry.create_session("token-clear").unwrap();
        registry
            .process_frame(now(), &notif("token-clear", 1, start()))
            .await
            .unwrap();

        registry.clear();
        assert_eq!(registry.size(), 0);
        // Dispose finalized the session gracefully.
        session.closed().await;
    }

    #[tokio::test]
    async fn accepts_start_frame_with_advisory_metadata_omitted() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(2));
        let outcome = registry
            .process_frame(now(), &notif("token-advisory", 1, start()))
            .await
            .unwrap();
        assert_eq!(outcome, FrameOutcome::None);
        assert!(registry.get_session("token-advisory").is_some());

        registry.clear();
        assert_eq!(registry.size(), 0);
    }

    #[tokio::test]
    async fn rejects_malformed_non_cep41_payloads() {
        // The structural predicate rejects every non-CEP-41 shape.
        let malformed = [
            json!({ "progressToken": "missing-cvm", "progress": 1 }),
            json!({ "progressToken": "wrong-type", "progress": 1, "cvm": { "type": "other", "frameType": "start" } }),
            json!({ "progressToken": "missing-frame-type", "progress": 1, "cvm": { "type": "open-stream" } }),
        ];
        for params in malformed {
            let notification = JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "notifications/progress".to_string(),
                params: Some(params),
            };
            assert!(!OpenStreamRegistry::is_open_stream_progress(&notification));
        }
        assert!(OpenStreamRegistry::is_open_stream_progress(&notif(
            "ok",
            1,
            start()
        )));

        // Feeding a malformed payload to the router is a Sequence error.
        let mut registry = OpenStreamRegistry::new();
        let bad = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({ "progressToken": "t", "progress": 1 })),
        };
        assert!(matches!(
            registry.process_frame(now(), &bad).await.unwrap_err(),
            OpenStreamError::Sequence(_)
        ));
    }

    #[tokio::test]
    async fn rejects_accept_as_the_first_frame() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(2));
        let err = registry
            .process_frame(
                now(),
                &notif("token-orphan-accept", 1, OpenStreamFrame::Accept),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
        assert!(registry.get_session("token-orphan-accept").is_none());
    }

    #[tokio::test]
    async fn removes_session_even_when_on_close_hook_errors() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(1));
        let on_close: RegistryCloseHook = Box::new(|| {
            Box::pin(async { Err::<(), Error>(Error::Other("close failed".to_string())) })
        });
        registry
            .create_session_with(
                "token-close-throws",
                OpenStreamSessionInit {
                    on_close: Some(on_close),
                    ..Default::default()
                },
            )
            .unwrap();

        registry
            .process_frame(now(), &notif("token-close-throws", 1, start()))
            .await
            .unwrap();
        // Graceful close fires on_close (which errors) but the session is removed.
        registry
            .process_frame(
                now(),
                &notif(
                    "token-close-throws",
                    2,
                    OpenStreamFrame::Close {
                        last_chunk_index: None,
                    },
                ),
            )
            .await
            .unwrap();

        assert!(registry.get_session("token-close-throws").is_none());
        assert_eq!(registry.size(), 0);
    }

    #[tokio::test]
    async fn consumer_abort_frees_slot_and_runs_hook() {
        // A consumer cancel must remove the registry entry (freeing the
        // concurrency slot) and run the `on_abort` hook, even though no inbound
        // terminal frame ever arrives.
        let mut registry = OpenStreamRegistry::with_policy(small_policy(1));
        let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let f = fired.clone();
        let on_abort: RegistryAbortHook = Box::new(move |reason| {
            let f = f.clone();
            Box::pin(async move {
                f.lock().unwrap().push(reason.unwrap_or_default());
                Ok(())
            })
        });
        let mut session = registry
            .create_session_with(
                "token-consumer-abort",
                OpenStreamSessionInit {
                    on_abort: Some(on_abort),
                    ..Default::default()
                },
            )
            .unwrap();
        // The slot is occupied: a second admission is rejected by the cap.
        assert!(matches!(
            registry.create_session("token-other").unwrap_err(),
            OpenStreamError::Policy(_)
        ));

        registry
            .consumer_abort("token-consumer-abort", Some("user cancelled".to_string()))
            .await;

        assert_eq!(registry.size(), 0);
        assert!(registry.get_session("token-consumer-abort").is_none());
        assert_eq!(*fired.lock().unwrap(), vec!["user cancelled".to_string()]);
        // The local stream surfaces the abort error on its next poll.
        match session.next().await {
            Some(Err(OpenStreamError::Abort { reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("user cancelled"));
            }
            other => panic!("expected abort error on the stream, got {other:?}"),
        }
        // Slot reclaimed: a fresh admission now succeeds.
        registry.create_session("token-other").unwrap();
    }

    #[tokio::test]
    async fn removes_session_even_when_on_abort_hook_errors() {
        let mut registry = OpenStreamRegistry::with_policy(small_policy(1));
        let on_abort: RegistryAbortHook = Box::new(|_reason| {
            Box::pin(async { Err::<(), Error>(Error::Other("abort failed".to_string())) })
        });
        registry
            .create_session_with(
                "token-abort-throws",
                OpenStreamSessionInit {
                    on_abort: Some(on_abort),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut session = registry.get_session("token-abort-throws").unwrap();

        registry
            .process_frame(now(), &notif("token-abort-throws", 1, start()))
            .await
            .unwrap();
        registry
            .process_frame(
                now(),
                &notif(
                    "token-abort-throws",
                    2,
                    OpenStreamFrame::Abort {
                        reason: Some("boom".to_string()),
                    },
                ),
            )
            .await
            .unwrap();

        assert!(registry.get_session("token-abort-throws").is_none());
        assert_eq!(registry.size(), 0);
        // The session was finalized with the peer's abort reason.
        match session.next().await {
            Some(Err(OpenStreamError::Abort { reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("boom"));
            }
            other => panic!("expected abort error on the stream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fires_only_on_close_on_graceful_close_and_only_on_abort_on_abort() {
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        // Build a (on_close, on_abort) pair that each record which hook fired,
        // tagged by the session, into one shared log.
        fn recording_hooks(
            events: StdArc<StdMutex<Vec<String>>>,
            token: &'static str,
        ) -> (RegistryCloseHook, RegistryAbortHook) {
            let close_events = events.clone();
            let on_close: RegistryCloseHook = Box::new(move || {
                Box::pin(async move {
                    close_events.lock().unwrap().push(format!("close:{token}"));
                    Ok(())
                })
            });
            let on_abort: RegistryAbortHook = Box::new(move |_reason| {
                Box::pin(async move {
                    events.lock().unwrap().push(format!("abort:{token}"));
                    Ok(())
                })
            });
            (on_close, on_abort)
        }

        let events = StdArc::new(StdMutex::new(Vec::<String>::new()));
        let mut registry = OpenStreamRegistry::with_policy(small_policy(4));

        // (1) A graceful close fires only `on_close`.
        let (on_close, on_abort) = recording_hooks(events.clone(), "graceful");
        registry
            .create_session_with(
                "tok-graceful",
                OpenStreamSessionInit {
                    on_close: Some(on_close),
                    on_abort: Some(on_abort),
                    ..Default::default()
                },
            )
            .unwrap();
        registry
            .process_frame(now(), &notif("tok-graceful", 1, start()))
            .await
            .unwrap();
        registry
            .process_frame(
                now(),
                &notif(
                    "tok-graceful",
                    2,
                    OpenStreamFrame::Close {
                        last_chunk_index: None,
                    },
                ),
            )
            .await
            .unwrap();

        // (2) An abort fires only `on_abort`.
        let (on_close, on_abort) = recording_hooks(events.clone(), "aborted");
        registry
            .create_session_with(
                "tok-aborted",
                OpenStreamSessionInit {
                    on_close: Some(on_close),
                    on_abort: Some(on_abort),
                    ..Default::default()
                },
            )
            .unwrap();
        registry
            .process_frame(now(), &notif("tok-aborted", 1, start()))
            .await
            .unwrap();
        registry
            .process_frame(
                now(),
                &notif(
                    "tok-aborted",
                    2,
                    OpenStreamFrame::Abort {
                        reason: Some("bye".to_string()),
                    },
                ),
            )
            .await
            .unwrap();

        // Exactly one hook fired per session, the right one each time.
        let fired = events.lock().unwrap().clone();
        assert_eq!(
            fired,
            vec!["close:graceful".to_string(), "abort:aborted".to_string()]
        );
    }
}
