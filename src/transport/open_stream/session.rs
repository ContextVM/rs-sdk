//! Reader-side view of a CEP-41 open stream.
//!
//! Ports `sdk/src/transport/open-stream/session.ts`. An [`OpenStreamSession`] is
//! fed inbound frames via the **synchronous** [`process_frame`](OpenStreamSession::process_frame)
//! and consumed as an async [`Stream`] of payload chunks. It owns no live timers:
//! the keepalive state machine is the pure [`tick`](OpenStreamSession::tick)
//! method (idle → `ping`, probe → abort, close-grace → abort), which the owning
//! transport drives from its periodic sweep.
//!
//! Design: the feed path is a plain sync call
//! under a `std::sync::Mutex`; the drain path is a manual [`Stream`] impl over a
//! `VecDeque` plus a stored [`Waker`]. The terminal error is delivered **once**
//! on the next poll (`terminal.take()`), an intentional divergence from the TS
//! reference where it is rejected to every parked reader; [`closed`](OpenStreamSession::closed)
//! resolves to `()` on any finalize and never carries the error.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::Stream;
use nostr_sdk::prelude::EventId;

use crate::core::types::JsonRpcNotification;

use super::errors::OpenStreamError;
use super::frame::OpenStreamFrame;

/// Injected outbound publish closure (same seam as the writer).
///
/// Used by the reader's consumer [`abort`](OpenStreamSession::abort) to emit an
/// `abort` frame to the peer. Cheaply clonable (`Arc`) so a session handle stays
/// clonable across the registry and the consumer.
pub type PublishFrame =
    Arc<dyn Fn(JsonRpcNotification) -> BoxFuture<'static, crate::Result<EventId>> + Send + Sync>;

/// The effect a single processed frame requires of the async caller.
///
/// Keeps the state machine I/O-free (like the oversized receiver): the inbound
/// dispatcher performs any send the outcome demands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameOutcome {
    /// No side effect; await more frames.
    None,
    /// A `ping` was received; the caller must publish a `pong` echoing the nonce.
    SendPong(String),
    /// The stream closed gracefully (terminal).
    Closed,
    /// The stream was aborted by the peer (terminal); advisory reason attached.
    Aborted(Option<String>),
}

/// The keepalive action a [`tick`](OpenStreamSession::tick) requires of the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepaliveAction {
    /// Idle threshold not yet crossed (or a probe is already in flight).
    None,
    /// Idle threshold crossed: publish a `ping` carrying this nonce.
    SendPing(String),
    /// A deadline expired: the stream was aborted locally with this reason.
    Abort(String),
}

/// Construction options for an [`OpenStreamSession`].
pub struct OpenStreamSessionOptions {
    /// The stream id (stringified `progressToken`).
    pub progress_token: String,
    /// Maximum buffered + queued chunks held before a `Sequence` error.
    pub max_buffered_chunks: usize,
    /// Maximum buffered + queued payload bytes before a `Sequence` error.
    pub max_buffered_bytes: u64,
    /// Idle interval before the reader probes the peer with a `ping` (ms).
    pub idle_timeout_ms: u64,
    /// Time the reader waits for a `pong` after probing before aborting (ms).
    pub probe_timeout_ms: u64,
    /// Grace period after a `close` with unresolved gaps before aborting (ms).
    pub close_grace_period_ms: u64,
    /// Outbound publisher for the consumer `abort` frame (optional).
    pub publish_frame: Option<PublishFrame>,
}

/// Mutable per-stream state, guarded by a `std::sync::Mutex`.
///
/// All mutation happens under the lock in synchronous methods that never
/// `.await`, so the lock is never held across a suspension point.
struct SessionState {
    progress_token: String,

    // ── policy ──────────────────────────────────────────────────────
    max_buffered_chunks: usize,
    max_buffered_bytes: u64,
    idle_timeout: Duration,
    probe_timeout: Duration,
    close_grace_period: Duration,

    // ── data plane ──────────────────────────────────────────────────
    /// Emittable chunks awaiting the consumer (FIFO).
    queue: VecDeque<String>,
    /// Out-of-order chunks buffered by `chunkIndex` until contiguous.
    buffered: BTreeMap<u64, String>,
    /// UTF-8 byte total of [`buffered`](Self::buffered).
    buffered_bytes: u64,
    /// UTF-8 byte total of [`queue`](Self::queue).
    queued_bytes: u64,
    /// The next contiguous `chunkIndex` to emit.
    next_expected_chunk: u64,
    /// Highest outer `progress` accepted so far (sentinel `-1`).
    last_progress: i64,
    started: bool,
    active: bool,
    closed_remotely: bool,
    /// `close.lastChunkIndex` (only meaningful once `closed_remotely`).
    expected_last_chunk_index: Option<u64>,
    /// Terminal error, delivered once on the next stream poll.
    terminal: Option<OpenStreamError>,
    /// Parked stream consumer.
    waker: Option<Waker>,
    /// Parked `closed()` futures.
    closed_wakers: Vec<Waker>,

    // ── keepalive (pure `tick`, driven by the transport sweep) ──────
    last_activity: Instant,
    pending_probe_nonce: Option<String>,
    control_nonce: u64,
    probe_deadline: Option<Instant>,
    close_grace_deadline: Option<Instant>,
    /// Outbound `progress` counter for the consumer `abort` frame.
    outbound_progress: u64,
}

impl SessionState {
    fn assert_active(&self) -> Result<(), OpenStreamError> {
        if !self.active {
            return Err(OpenStreamError::Sequence(format!(
                "Received frame for inactive stream {}",
                self.progress_token
            )));
        }
        Ok(())
    }

    fn assert_started(&self) -> Result<(), OpenStreamError> {
        if !self.started {
            return Err(OpenStreamError::Sequence(format!(
                "Received non-start frame before start for {}",
                self.progress_token
            )));
        }
        Ok(())
    }

    /// The outer `progress` must be strictly increasing across all frames.
    fn assert_progress(&mut self, progress: i64) -> Result<(), OpenStreamError> {
        if progress <= self.last_progress {
            return Err(OpenStreamError::Sequence(format!(
                "Non-increasing progress for stream {}",
                self.progress_token
            )));
        }
        self.last_progress = progress;
        Ok(())
    }

    fn buffer_chunk(&mut self, chunk_index: u64, data: String) -> Result<(), OpenStreamError> {
        if chunk_index < self.next_expected_chunk {
            return Err(OpenStreamError::Sequence(format!(
                "Stale chunkIndex {chunk_index} for {}",
                self.progress_token
            )));
        }
        if self.buffered.contains_key(&chunk_index) {
            return Err(OpenStreamError::Sequence(format!(
                "Duplicate chunkIndex {chunk_index} for {}",
                self.progress_token
            )));
        }

        let chunk_bytes = data.len() as u64;
        if self.buffered.len() + self.queue.len() >= self.max_buffered_chunks {
            return Err(OpenStreamError::Sequence(format!(
                "Buffered chunk limit exceeded for stream {}",
                self.progress_token
            )));
        }
        if self.buffered_bytes + self.queued_bytes + chunk_bytes > self.max_buffered_bytes {
            return Err(OpenStreamError::Sequence(format!(
                "Buffered byte limit exceeded for stream {}",
                self.progress_token
            )));
        }

        self.buffered.insert(chunk_index, data);
        self.buffered_bytes += chunk_bytes;
        Ok(())
    }

    /// Flush every contiguous buffered chunk into the emit queue, then — if the
    /// peer has closed — attempt a graceful finish. Returns `Ok(true)` when the
    /// stream finished gracefully on this call.
    fn flush_contiguous_chunks(&mut self) -> Result<bool, OpenStreamError> {
        while let Some(data) = self.buffered.remove(&self.next_expected_chunk) {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(data.len() as u64);
            self.emit(data);
            self.next_expected_chunk += 1;
        }

        if self.closed_remotely {
            return self.maybe_finish_gracefully();
        }
        Ok(false)
    }

    /// Resolve a graceful close if all declared chunks have been emitted.
    /// Returns `Ok(true)` on finish, `Ok(false)` while still waiting on a gap,
    /// `Err(Sequence)` when the declared `lastChunkIndex` can never be met.
    fn maybe_finish_gracefully(&mut self) -> Result<bool, OpenStreamError> {
        if !self.closed_remotely || !self.buffered.is_empty() {
            return Ok(false);
        }

        if let Some(expected) = self.expected_last_chunk_index {
            if self.next_expected_chunk != expected + 1 {
                return Err(OpenStreamError::Sequence(format!(
                    "Incomplete stream for {}: expected chunks through {expected}",
                    self.progress_token
                )));
            }
        }

        self.finalize(None);
        Ok(true)
    }

    /// Push an emittable chunk and wake the parked consumer.
    fn emit(&mut self, data: String) {
        self.queued_bytes += data.len() as u64;
        self.queue.push_back(data);
        self.wake_stream();
    }

    fn handle_pong(&mut self, nonce: &str) {
        if self.pending_probe_nonce.as_deref() == Some(nonce) {
            self.pending_probe_nonce = None;
            self.probe_deadline = None;
        }
    }

    fn next_control_nonce(&mut self) -> String {
        self.control_nonce += 1;
        format!("{}:{}", self.progress_token, self.control_nonce)
    }

    /// Pure keepalive transition. The owning transport calls this from its sweep
    /// with `Instant::now()` and performs any returned send.
    fn tick(&mut self, now: Instant) -> KeepaliveAction {
        if !self.active {
            return KeepaliveAction::None;
        }

        // 1. A pending probe whose deadline passed → abort.
        if let (Some(deadline), true) = (self.probe_deadline, self.pending_probe_nonce.is_some()) {
            if now >= deadline {
                let token = self.progress_token.clone();
                self.finalize(Some(OpenStreamError::abort(
                    token,
                    Some("Probe timeout".to_string()),
                )));
                return KeepaliveAction::Abort("Probe timeout".to_string());
            }
        }

        // 2. Close-grace deadline passed with chunks still missing → abort.
        if let Some(deadline) = self.close_grace_deadline {
            if self.closed_remotely && !self.buffered.is_empty() && now >= deadline {
                let token = self.progress_token.clone();
                self.finalize(Some(OpenStreamError::abort(
                    token,
                    Some("Close grace period expired".to_string()),
                )));
                return KeepaliveAction::Abort("Close grace period expired".to_string());
            }
        }

        // 3. Idle past the threshold (and not closed / not already probing) → ping.
        if !self.closed_remotely
            && self.pending_probe_nonce.is_none()
            && now.saturating_duration_since(self.last_activity) >= self.idle_timeout
        {
            let nonce = self.next_control_nonce();
            self.pending_probe_nonce = Some(nonce.clone());
            self.probe_deadline = Some(now + self.probe_timeout);
            return KeepaliveAction::SendPing(nonce);
        }

        KeepaliveAction::None
    }

    /// Terminate the stream. Idempotent. `error` is delivered once on the next
    /// stream poll; `None` ends the stream gracefully. Wakes the consumer and
    /// any `closed()` futures.
    fn finalize(&mut self, error: Option<OpenStreamError>) {
        if !self.active {
            return;
        }
        self.active = false;
        self.terminal = error;
        self.pending_probe_nonce = None;
        self.probe_deadline = None;
        self.close_grace_deadline = None;
        self.wake_stream();
        for waker in self.closed_wakers.drain(..) {
            waker.wake();
        }
    }

    fn wake_stream(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

/// Readable reader-side handle for a CEP-41 stream.
///
/// Cheaply clonable (`Arc`-backed): the registry holds one clone to feed frames
/// while the consumer holds another to drain the [`Stream`].
#[derive(Clone)]
pub struct OpenStreamSession {
    /// Immutable stream id, duplicated here so accessors/`Debug` need no lock.
    progress_token: String,
    state: Arc<Mutex<SessionState>>,
    publish_frame: Option<PublishFrame>,
}

impl std::fmt::Debug for OpenStreamSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenStreamSession")
            .field("progress_token", &self.progress_token)
            .finish_non_exhaustive()
    }
}

impl OpenStreamSession {
    /// Create a new reader session from explicit options.
    pub fn new(options: OpenStreamSessionOptions) -> Self {
        let publish_frame = options.publish_frame.clone();
        let progress_token = options.progress_token.clone();
        let state = SessionState {
            progress_token: options.progress_token,
            max_buffered_chunks: options.max_buffered_chunks,
            max_buffered_bytes: options.max_buffered_bytes,
            idle_timeout: Duration::from_millis(options.idle_timeout_ms),
            probe_timeout: Duration::from_millis(options.probe_timeout_ms),
            close_grace_period: Duration::from_millis(options.close_grace_period_ms),
            queue: VecDeque::new(),
            buffered: BTreeMap::new(),
            buffered_bytes: 0,
            queued_bytes: 0,
            next_expected_chunk: 0,
            last_progress: -1,
            started: false,
            active: true,
            closed_remotely: false,
            expected_last_chunk_index: None,
            terminal: None,
            waker: None,
            closed_wakers: Vec::new(),
            last_activity: Instant::now(),
            pending_probe_nonce: None,
            control_nonce: 0,
            probe_deadline: None,
            close_grace_deadline: None,
            outbound_progress: 0,
        };
        Self {
            progress_token,
            state: Arc::new(Mutex::new(state)),
            publish_frame,
        }
    }

    /// The stream id (stringified `progressToken`).
    pub fn progress_token(&self) -> &str {
        &self.progress_token
    }

    /// Whether the stream is still live (not yet closed/aborted/failed).
    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap().active
    }

    /// Whether the `start` frame has been observed.
    pub fn has_started(&self) -> bool {
        self.state.lock().unwrap().started
    }

    /// Process one inbound frame, stamping `last_activity = now`.
    ///
    /// Returns the [`FrameOutcome`] the async caller must act on (publish a
    /// `pong`, observe a terminal close/abort). On a sequencing/policy violation
    /// it returns `Err` **without** finalizing — the caller (registry) decides
    /// whether to [`fail`](Self::fail) the session.
    pub fn process_frame(
        &self,
        now: Instant,
        progress: i64,
        frame: OpenStreamFrame,
    ) -> Result<FrameOutcome, OpenStreamError> {
        let mut s = self.state.lock().unwrap();
        s.assert_active()?;
        s.assert_progress(progress)?;
        // Every accepted frame counts as liveness.
        s.last_activity = now;

        match frame {
            OpenStreamFrame::Start { .. } => {
                if s.started {
                    return Err(OpenStreamError::Sequence(format!(
                        "Duplicate start frame for stream {}",
                        s.progress_token
                    )));
                }
                s.started = true;
                Ok(FrameOutcome::None)
            }
            OpenStreamFrame::Accept => Ok(FrameOutcome::None),
            OpenStreamFrame::Ping { nonce } => {
                s.assert_started()?;
                Ok(FrameOutcome::SendPong(nonce))
            }
            OpenStreamFrame::Pong { nonce } => {
                s.assert_started()?;
                s.handle_pong(&nonce);
                Ok(FrameOutcome::None)
            }
            OpenStreamFrame::Chunk { chunk_index, data } => {
                s.assert_started()?;
                s.buffer_chunk(chunk_index, data)?;
                let finished = s.flush_contiguous_chunks()?;
                Ok(if finished {
                    FrameOutcome::Closed
                } else {
                    FrameOutcome::None
                })
            }
            OpenStreamFrame::Close { last_chunk_index } => {
                s.assert_started()?;
                s.closed_remotely = true;
                s.expected_last_chunk_index = last_chunk_index;
                let finished = s.flush_contiguous_chunks()?;
                if finished {
                    Ok(FrameOutcome::Closed)
                } else {
                    // A real out-of-order gap remains: arm the close grace timer
                    // (only reached when chunks are still buffered).
                    s.close_grace_deadline = Some(now + s.close_grace_period);
                    Ok(FrameOutcome::None)
                }
            }
            OpenStreamFrame::Abort { reason } => {
                let token = s.progress_token.clone();
                s.finalize(Some(OpenStreamError::abort(token, reason.clone())));
                Ok(FrameOutcome::Aborted(reason))
            }
        }
    }

    /// Pure keepalive transition (idle → `ping`, probe/grace → abort). Driven by
    /// the owning transport's periodic sweep with `Instant::now()`.
    pub fn tick(&self, now: Instant) -> KeepaliveAction {
        self.state.lock().unwrap().tick(now)
    }

    /// Terminate the stream with an explicit error (no abort frame published).
    /// Mirrors the registry's `fail` path; the error surfaces on the next poll.
    pub fn fail(&self, error: OpenStreamError) {
        self.state.lock().unwrap().finalize(Some(error));
    }

    /// Dispose the stream gracefully (ends the [`Stream`] with `None`). Used by
    /// the registry's `clear`; runs no hooks.
    pub fn dispose(&self) {
        self.state.lock().unwrap().finalize(None);
    }

    /// Consumer-initiated cancel: finalize locally (terminal abort error on the
    /// next poll) and, if a publisher was injected, emit an `abort` frame to the
    /// peer. Idempotent; the transport exposes this as the consumer's stream cancel.
    pub async fn abort(&self, reason: Option<String>) {
        let publish = {
            let mut s = self.state.lock().unwrap();
            if !s.active {
                return;
            }
            let token = s.progress_token.clone();
            s.finalize(Some(OpenStreamError::abort(token.clone(), reason.clone())));
            self.publish_frame.as_ref().map(|publisher| {
                s.outbound_progress += 1;
                (publisher.clone(), token, s.outbound_progress)
            })
        };

        if let Some((publisher, token, progress)) = publish {
            if let Ok(notification) = (OpenStreamFrame::Abort {
                reason: reason.clone(),
            })
            .into_progress_notification(&token, progress, None)
            {
                // Best effort — the local stream is already finalized.
                let _ = publisher(notification).await;
            }
        }
    }

    /// A future that resolves to `()` when the stream finalizes (graceful close,
    /// abort, fail, or dispose). It never carries the terminal error — that is
    /// delivered once on the [`Stream`] (intentional Rust divergence from TS).
    pub fn closed(&self) -> Closed {
        Closed {
            state: self.state.clone(),
        }
    }

    /// Whether two handles refer to the same underlying session state (used by
    /// the registry's get-or-create reuse tests).
    #[cfg(test)]
    pub(crate) fn shares_state_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}

impl Stream for OpenStreamSession {
    type Item = Result<String, OpenStreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut s = self.state.lock().unwrap();
        if let Some(item) = s.queue.pop_front() {
            s.queued_bytes = s.queued_bytes.saturating_sub(item.len() as u64);
            return Poll::Ready(Some(Ok(item)));
        }
        if !s.active {
            return match s.terminal.take() {
                Some(error) => Poll::Ready(Some(Err(error))),
                None => Poll::Ready(None),
            };
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Future returned by [`OpenStreamSession::closed`]; resolves on finalize.
pub struct Closed {
    state: Arc<Mutex<SessionState>>,
}

impl Future for Closed {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = self.state.lock().unwrap();
        if !s.active {
            Poll::Ready(())
        } else {
            s.closed_wakers.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{poll, StreamExt};

    fn make_session(token: &str, max_chunks: usize, max_bytes: u64) -> OpenStreamSession {
        OpenStreamSession::new(OpenStreamSessionOptions {
            progress_token: token.to_string(),
            max_buffered_chunks: max_chunks,
            max_buffered_bytes: max_bytes,
            idle_timeout_ms: 30_000,
            probe_timeout_ms: 20_000,
            close_grace_period_ms: 5_000,
            publish_frame: None,
        })
    }

    fn make_session_timers(token: &str, idle: u64, probe: u64, grace: u64) -> OpenStreamSession {
        OpenStreamSession::new(OpenStreamSessionOptions {
            progress_token: token.to_string(),
            max_buffered_chunks: 8,
            max_buffered_bytes: 1024,
            idle_timeout_ms: idle,
            probe_timeout_ms: probe,
            close_grace_period_ms: grace,
            publish_frame: None,
        })
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

    fn close(last: Option<u64>) -> OpenStreamFrame {
        OpenStreamFrame::Close {
            last_chunk_index: last,
        }
    }

    /// Drain a *finalized* stream's queued chunks (panics on a terminal error).
    async fn drain_ok(session: &mut OpenStreamSession) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(item) = session.next().await {
            out.push(item.expect("graceful stream must not yield an error"));
        }
        out
    }

    // ── data-plane ──────────────────────────────────────────────────

    #[tokio::test]
    async fn yields_ordered_chunks_and_finishes_after_close() {
        let now = Instant::now();
        let mut s = make_session("token-1", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(0, "hello")).unwrap();
        s.process_frame(now, 3, chunk(1, " world")).unwrap();
        assert_eq!(
            s.process_frame(now, 4, close(None)).unwrap(),
            FrameOutcome::Closed
        );

        assert_eq!(drain_ok(&mut s).await, vec!["hello", " world"]);
        s.closed().await;
    }

    #[tokio::test]
    async fn buffers_out_of_order_chunks_until_contiguous() {
        let now = Instant::now();
        let mut s = make_session("token-2", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(1, "world")).unwrap();
        s.process_frame(now, 3, chunk(0, "hello ")).unwrap();
        s.process_frame(now, 4, close(None)).unwrap();

        assert_eq!(drain_ok(&mut s).await, vec!["hello ", "world"]);
    }

    #[tokio::test]
    async fn fails_when_progress_does_not_increase() {
        let now = Instant::now();
        let s = make_session("token-3", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        let err = s.process_frame(now, 1, chunk(0, "repeat")).unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn rejects_a_duplicate_start_frame() {
        let now = Instant::now();
        let s = make_session("token-dup-start", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        // A second `start` on an already-started stream is a sequencing violation.
        let err = s.process_frame(now, 2, start()).unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn abort_frame_terminates_stream_with_error_on_next_poll() {
        let now = Instant::now();
        let mut s = make_session("token-4", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        assert_eq!(
            s.process_frame(
                now,
                2,
                OpenStreamFrame::Abort {
                    reason: Some("boom".to_string())
                }
            )
            .unwrap(),
            FrameOutcome::Aborted(Some("boom".to_string()))
        );

        match s.next().await {
            Some(Err(OpenStreamError::Abort { reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("boom"));
            }
            other => panic!("expected abort error, got {other:?}"),
        }
        // The error is delivered once; the stream then ends.
        assert!(s.next().await.is_none());
        s.closed().await;
    }

    #[tokio::test]
    async fn fails_when_buffered_chunk_count_exceeds_limit() {
        let now = Instant::now();
        let s = make_session("token-buffer-count", 1, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(1, "late")).unwrap();
        let err = s.process_frame(now, 3, chunk(2, "later")).unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn fails_when_buffered_byte_count_exceeds_limit_utf8() {
        let now = Instant::now();
        let s = make_session("token-buffer-bytes", 4, 4);
        s.process_frame(now, 1, start()).unwrap();
        // "héllo" is 6 UTF-8 bytes (é = 2), exceeding the 4-byte cap; this also
        // pins UTF-8 byte accounting (a `str::len` over chars would miscount).
        let err = s.process_frame(now, 2, chunk(1, "héllo")).unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn fail_terminates_stream_and_closed_resolves() {
        let now = Instant::now();
        let mut s = make_session("token-explicit-fail", 4, 16);
        s.process_frame(now, 1, start()).unwrap();
        s.fail(OpenStreamError::Sequence("synthetic failure".to_string()));

        match s.next().await {
            Some(Err(OpenStreamError::Sequence(msg))) => assert!(msg.contains("synthetic")),
            other => panic!("expected sequence error, got {other:?}"),
        }
        // `closed()` resolves to `()` (the error rode the stream, not `closed`).
        s.closed().await;
    }

    #[tokio::test]
    async fn counts_unread_queued_chunks_against_the_byte_limit() {
        let now = Instant::now();
        let s = make_session("token-queued-bytes", 4, 5);
        s.process_frame(now, 1, start()).unwrap();
        // chunk 0 flushes immediately → 3 queued bytes counted against the cap.
        s.process_frame(now, 2, chunk(0, "abc")).unwrap();
        let err = s.process_frame(now, 3, chunk(1, "def")).unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn releases_queued_byte_budget_after_consume() {
        let now = Instant::now();
        let mut s = make_session("token-queued-release", 4, 6);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(0, "abc")).unwrap();
        s.process_frame(now, 3, chunk(1, "def")).unwrap();
        // 3 + 3 + 1 = 7 > 6 → rejected (progress 4 is consumed by assert_progress).
        assert!(matches!(
            s.process_frame(now, 4, chunk(2, "g")).unwrap_err(),
            OpenStreamError::Sequence(_)
        ));

        // Consuming "abc" frees 3 bytes, so the retry (progress 5) now fits.
        assert_eq!(s.next().await.unwrap().unwrap(), "abc");
        s.process_frame(now, 5, chunk(2, "g")).unwrap();
    }

    #[tokio::test]
    async fn rejects_frames_after_close() {
        let now = Instant::now();
        let s = make_session("token-post-close", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        assert_eq!(
            s.process_frame(now, 2, close(None)).unwrap(),
            FrameOutcome::Closed
        );
        let err = s.process_frame(now, 3, chunk(0, "late")).unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn rejects_frames_after_abort() {
        let now = Instant::now();
        let s = make_session("token-post-abort", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(
            now,
            2,
            OpenStreamFrame::Abort {
                reason: Some("boom".to_string()),
            },
        )
        .unwrap();
        assert!(matches!(
            s.process_frame(now, 3, chunk(0, "late")).unwrap_err(),
            OpenStreamError::Sequence(_)
        ));
        assert!(matches!(
            s.process_frame(now, 4, close(None)).unwrap_err(),
            OpenStreamError::Sequence(_)
        ));
    }

    #[tokio::test]
    async fn rejects_stale_and_duplicate_chunk_indexes() {
        let now = Instant::now();
        // Stale: an index below the contiguous frontier (already flushed).
        let s = make_session("token-stale", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(0, "hello")).unwrap();
        assert!(matches!(
            s.process_frame(now, 3, chunk(0, "late-duplicate"))
                .unwrap_err(),
            OpenStreamError::Sequence(_)
        ));

        // Duplicate: a still-buffered out-of-order index re-delivered.
        let s2 = make_session("token-dup", 8, 1024);
        s2.process_frame(now, 1, start()).unwrap();
        s2.process_frame(now, 2, chunk(2, "a")).unwrap();
        assert!(matches!(
            s2.process_frame(now, 3, chunk(2, "b")).unwrap_err(),
            OpenStreamError::Sequence(_)
        ));
    }

    #[tokio::test]
    async fn requires_all_chunks_through_close_last_chunk_index() {
        let now = Instant::now();
        let s = make_session("token-last-chunk-index", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(0, "hello")).unwrap();
        let err = s.process_frame(now, 3, close(Some(1))).unwrap_err();
        assert!(matches!(err, OpenStreamError::Sequence(_)));
    }

    #[tokio::test]
    async fn allows_graceful_close_when_last_chunk_index_matches() {
        let now = Instant::now();
        let mut s = make_session("token-last-chunk-complete", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(0, "hello")).unwrap();
        assert_eq!(
            s.process_frame(now, 3, close(Some(0))).unwrap(),
            FrameOutcome::Closed
        );
        assert_eq!(drain_ok(&mut s).await, vec!["hello"]);
        s.closed().await;
    }

    #[tokio::test]
    async fn rejects_malformed_close_last_chunk_index_and_stays_active() {
        let now = Instant::now();
        let mut s = make_session("token-malformed-last", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        s.process_frame(now, 2, chunk(0, "hello")).unwrap();
        // lastChunkIndex far larger than anything received → Sequence, but the
        // stream is not finalized (a consumer abort can still clean it up).
        assert!(matches!(
            s.process_frame(now, 3, close(Some(5))).unwrap_err(),
            OpenStreamError::Sequence(_)
        ));
        assert!(s.is_active());

        s.abort(Some("cleanup".to_string())).await;
        // The already-flushed "hello" drains before the terminal abort error.
        assert_eq!(s.next().await.unwrap().unwrap(), "hello");
        match s.next().await {
            Some(Err(OpenStreamError::Abort { reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("cleanup"));
            }
            other => panic!("expected abort error, got {other:?}"),
        }
        s.closed().await;
    }

    #[tokio::test]
    async fn rejects_negative_chunk_index_and_stays_active() {
        use crate::transport::open_stream::OpenStreamRegistry;
        use serde_json::json;

        // A `chunk` whose `chunkIndex` is a negative JSON number is not a valid
        // CEP-41 frame: `chunk_index` is a `u64`, so it fails to deserialize at
        // the wire boundary and never becomes a typed `Chunk`. A non-integer
        // index is rejected the same way.
        let negative = json!({
            "type": "open-stream",
            "frameType": "chunk",
            "chunkIndex": -1,
            "data": "oops",
        });
        assert_eq!(OpenStreamFrame::from_cvm_value(&negative), None);
        let fractional = json!({
            "type": "open-stream",
            "frameType": "chunk",
            "chunkIndex": 1.5,
            "data": "oops",
        });
        assert_eq!(OpenStreamFrame::from_cvm_value(&fractional), None);

        // End to end: unlike a malformed-but-typed `lastChunkIndex` (see
        // `rejects_malformed_close_last_chunk_index_and_stays_active`, which the
        // session itself rejects), a negative index is rejected one layer out at
        // the registry's `parse_frame` boundary with a `Sequence` error, and the
        // already-admitted stream is left active (cleanable via a later frame).
        let now = Instant::now();
        let mut registry = OpenStreamRegistry::new();
        registry
            .process_frame(
                now,
                &start()
                    .into_progress_notification("tok-neg", 1, None)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registry.size(), 1);

        let bad = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({
                "progressToken": "tok-neg",
                "progress": 2,
                "cvm": negative,
            })),
        };
        assert!(matches!(
            registry.process_frame(now, &bad).await.unwrap_err(),
            OpenStreamError::Sequence(_)
        ));

        // The malformed frame neither terminated nor removed the stream.
        assert_eq!(registry.size(), 1);
        assert!(registry.get_session("tok-neg").unwrap().is_active());
    }

    #[tokio::test]
    async fn zero_chunk_close_succeeds() {
        let now = Instant::now();
        let mut s = make_session("token-zero-chunk", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        assert_eq!(
            s.process_frame(now, 2, close(None)).unwrap(),
            FrameOutcome::Closed
        );
        assert!(s.next().await.is_none());
        s.closed().await;
    }

    #[tokio::test]
    async fn chunk_filling_last_gap_after_close_yields_closed() {
        let now = Instant::now();
        let mut s = make_session("token-fill-gap", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        // Out-of-order chunk 1 buffered; close declares 1 is the last chunk.
        s.process_frame(now, 2, chunk(1, "world")).unwrap();
        assert_eq!(
            s.process_frame(now, 3, close(Some(1))).unwrap(),
            FrameOutcome::None
        );
        // Chunk 0 fills the gap → both flush and the stream closes.
        assert_eq!(
            s.process_frame(now, 4, chunk(0, "hello ")).unwrap(),
            FrameOutcome::Closed
        );
        assert_eq!(drain_ok(&mut s).await, vec!["hello ", "world"]);
    }

    #[tokio::test]
    async fn parked_reader_is_woken_by_an_arriving_chunk() {
        let now = Instant::now();
        let mut s = make_session("token-park", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        // No data yet → the manual Stream impl parks (Pending), storing the waker.
        assert!(poll!(s.next()).is_pending());
        s.process_frame(now, 2, chunk(0, "x")).unwrap();
        match poll!(s.next()) {
            Poll::Ready(Some(Ok(value))) => assert_eq!(value, "x"),
            other => panic!("expected the queued chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn consumer_abort_publishes_an_abort_frame_then_finalizes() {
        use nostr_sdk::prelude::EventId;

        let now = Instant::now();
        let published: Arc<Mutex<Vec<JsonRpcNotification>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = published.clone();
        let publish: PublishFrame = Arc::new(move |frame: JsonRpcNotification| {
            let recorder = recorder.clone();
            Box::pin(async move {
                recorder.lock().unwrap().push(frame);
                Ok(EventId::all_zeros())
            })
        });
        let mut s = OpenStreamSession::new(OpenStreamSessionOptions {
            progress_token: "token-consumer-abort".to_string(),
            max_buffered_chunks: 8,
            max_buffered_bytes: 1024,
            idle_timeout_ms: 30_000,
            probe_timeout_ms: 20_000,
            close_grace_period_ms: 5_000,
            publish_frame: Some(publish),
        });
        s.process_frame(now, 1, start()).unwrap();

        s.abort(Some("user cancelled".to_string())).await;

        // The injected publisher emitted exactly one `abort` frame to the peer.
        {
            let frames = published.lock().unwrap();
            assert_eq!(frames.len(), 1);
            let cvm = &frames[0].params.as_ref().unwrap()["cvm"];
            assert_eq!(cvm["frameType"], "abort");
            assert_eq!(cvm["reason"], "user cancelled");
        }

        // And the local stream is finalized with the abort error.
        match s.next().await {
            Some(Err(OpenStreamError::Abort { reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("user cancelled"));
            }
            other => panic!("expected abort error, got {other:?}"),
        }
    }

    // ── keepalive (pure `tick`, injected clock) ─────────────────────

    #[tokio::test]
    async fn ping_frame_requests_a_pong() {
        let now = Instant::now();
        let s = make_session("token-ping", 8, 1024);
        s.process_frame(now, 1, start()).unwrap();
        assert_eq!(
            s.process_frame(
                now,
                2,
                OpenStreamFrame::Ping {
                    nonce: "nonce-1".to_string()
                }
            )
            .unwrap(),
            FrameOutcome::SendPong("nonce-1".to_string())
        );
    }

    #[tokio::test]
    async fn idle_sends_ping_then_probe_timeout_aborts() {
        let t0 = Instant::now();
        let mut s = make_session_timers("token-probe", 10, 10, 100);
        s.process_frame(t0, 1, start()).unwrap();

        // Before the idle threshold: nothing.
        assert_eq!(s.tick(t0 + Duration::from_millis(5)), KeepaliveAction::None);
        // At the idle threshold: probe with a `{token}:{n}` nonce.
        assert_eq!(
            s.tick(t0 + Duration::from_millis(10)),
            KeepaliveAction::SendPing("token-probe:1".to_string())
        );
        // Probe in flight, deadline (t0+20) not yet reached: nothing.
        assert_eq!(
            s.tick(t0 + Duration::from_millis(15)),
            KeepaliveAction::None
        );
        // Probe deadline reached with no pong: abort.
        assert_eq!(
            s.tick(t0 + Duration::from_millis(20)),
            KeepaliveAction::Abort("Probe timeout".to_string())
        );

        match s.next().await {
            Some(Err(OpenStreamError::Abort { reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("Probe timeout"));
            }
            other => panic!("expected probe-timeout abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn matching_pong_clears_probe_and_stream_survives() {
        let t0 = Instant::now();
        let s = make_session_timers("token-probe-ok", 10, 20, 100);
        s.process_frame(t0, 1, start()).unwrap();

        let nonce = match s.tick(t0 + Duration::from_millis(10)) {
            KeepaliveAction::SendPing(nonce) => nonce,
            other => panic!("expected a ping, got {other:?}"),
        };
        // A matching pong clears the probe and refreshes liveness.
        s.process_frame(
            t0 + Duration::from_millis(15),
            2,
            OpenStreamFrame::Pong { nonce },
        )
        .unwrap();

        // At the original probe deadline the stream is NOT aborted.
        assert!(!matches!(
            s.tick(t0 + Duration::from_millis(30)),
            KeepaliveAction::Abort(_)
        ));
        assert!(s.is_active());
    }

    #[tokio::test]
    async fn unexpected_pong_is_ignored_for_liveness() {
        let t0 = Instant::now();
        let mut s = make_session_timers("token-bad-pong", 10, 10, 100);
        s.process_frame(t0, 1, start()).unwrap();
        assert!(matches!(
            s.tick(t0 + Duration::from_millis(10)),
            KeepaliveAction::SendPing(_)
        ));

        // A pong with a non-matching nonce must not clear the pending probe.
        s.process_frame(
            t0 + Duration::from_millis(12),
            2,
            OpenStreamFrame::Pong {
                nonce: "unexpected".to_string(),
            },
        )
        .unwrap();

        assert_eq!(
            s.tick(t0 + Duration::from_millis(20)),
            KeepaliveAction::Abort("Probe timeout".to_string())
        );
        match s.next().await {
            Some(Err(OpenStreamError::Abort { .. })) => {}
            other => panic!("expected abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_grace_deadline_with_missing_chunks_aborts() {
        let t0 = Instant::now();
        let mut s = make_session_timers("token-grace", 100, 100, 10);
        s.process_frame(t0, 1, start()).unwrap();
        // Out-of-order chunk buffered, then a close with the gap unresolved.
        s.process_frame(t0, 2, chunk(1, "late")).unwrap();
        assert_eq!(
            s.process_frame(t0, 3, close(None)).unwrap(),
            FrameOutcome::None
        );

        // Before the grace deadline: nothing. At it: abort.
        assert_eq!(s.tick(t0 + Duration::from_millis(5)), KeepaliveAction::None);
        assert_eq!(
            s.tick(t0 + Duration::from_millis(10)),
            KeepaliveAction::Abort("Close grace period expired".to_string())
        );
        match s.next().await {
            Some(Err(OpenStreamError::Abort { reason, .. })) => {
                assert_eq!(reason.as_deref(), Some("Close grace period expired"));
            }
            other => panic!("expected grace abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_with_missing_tail_and_nothing_buffered_fails_immediately_without_grace() {
        let t0 = Instant::now();
        let s = make_session_timers("token-no-grace", 100, 100, 10);
        s.process_frame(t0, 1, start()).unwrap();
        s.process_frame(t0, 2, chunk(0, "hello")).unwrap();
        // Tail declared (lastChunkIndex=1) but chunk 1 never arrived and nothing
        // is buffered → immediate Sequence error, no grace timer armed.
        assert!(matches!(
            s.process_frame(t0, 3, close(Some(1))).unwrap_err(),
            OpenStreamError::Sequence(_)
        ));
        // No close-grace deadline was armed: even far in the future, tick is inert.
        assert_eq!(s.tick(t0 + Duration::from_secs(60)), KeepaliveAction::None);
    }
}
