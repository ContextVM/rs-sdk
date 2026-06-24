//! Producer-side writer for a CEP-41 open stream.
//!
//! Ports `sdk/src/transport/open-stream/writer.ts`. A tool emits an ordered
//! sequence of `chunk` frames (plus keepalive `ping`/`pong` and a terminal
//! `close`/`abort`) through an injected publish closure.
//!
//! Serialization design: `write`/`close`/`ping`/`pong`
//! hold a `tokio::sync::Mutex` across their publish `.await`, so **call order ==
//! wire order** natively (each op increments `progress`/`chunkIndex` under the
//! lock). The liveness flag lives in a **separate `AtomicBool` outside that
//! lock**, so [`abort`](OpenStreamWriter::abort) can claim the terminal
//! transition and publish without queueing behind a stuck `write`.
//!
//! The handle is `Arc`-backed and `Clone` so it can be inserted into the rmcp
//! request `extensions` typemap (`T: Clone + Send + Sync + 'static`) when the
//! server transport wiring lands.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::Mutex;

use super::frame::OpenStreamFrame;
use super::session::PublishFrame;

/// Lifecycle hook fired after a terminal `close` frame is published.
///
/// Currently inert; the server transport will wire it to flush a deferred final response.
pub type OnCloseHook = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Lifecycle hook fired after a terminal `abort` frame is published (with the
/// advisory reason).
///
/// Currently inert; the server transport will wire it to flush a deferred final response.
pub type OnAbortHook = Arc<dyn Fn(Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Construction options for an [`OpenStreamWriter`].
pub struct OpenStreamWriterOptions {
    /// The stream id (stringified `progressToken`).
    pub progress_token: String,
    /// Outbound publisher (same seam as the session).
    pub publish_frame: PublishFrame,
    /// Optional advisory `start` content type (writer-settable, receiver-ignored).
    pub content_type: Option<String>,
    /// Fired after a terminal `close` frame is published.
    pub on_close: Option<OnCloseHook>,
    /// Fired after a terminal `abort` frame is published.
    pub on_abort: Option<OnAbortHook>,
}

/// State mutated only under the op `Mutex` (serialized publishes).
struct WriterOpState {
    /// Next `chunkIndex` to assign (touched only by `write`).
    chunk_index: u64,
    /// Control-frame nonce counter (touched only by `ping`).
    control_nonce: u64,
}

struct WriterInner {
    progress_token: String,
    content_type: Option<String>,
    publish_frame: PublishFrame,
    on_close: Option<OnCloseHook>,
    on_abort: Option<OnAbortHook>,
    /// Serializes `write`/`close`/`ping`/`pong` so call order == wire order.
    op: Mutex<WriterOpState>,
    /// Monotonic outer `progress`, shared so `abort` can mint one without the op
    /// lock. Frames use `fetch_add(1) + 1` → 1, 2, 3, …
    progress: AtomicU64,
    /// Liveness flag, **outside** the op lock so `abort` never blocks on a stuck
    /// write. `false` once closed or aborted.
    active: AtomicBool,
    /// Whether the lazy `start` frame has been published.
    started: AtomicBool,
}

/// Minimal CEP-41 producer/writer.
#[derive(Clone)]
pub struct OpenStreamWriter {
    inner: Arc<WriterInner>,
}

impl OpenStreamWriter {
    /// Create a new writer from explicit options.
    pub fn new(options: OpenStreamWriterOptions) -> Self {
        Self {
            inner: Arc::new(WriterInner {
                progress_token: options.progress_token,
                content_type: options.content_type,
                publish_frame: options.publish_frame,
                on_close: options.on_close,
                on_abort: options.on_abort,
                op: Mutex::new(WriterOpState {
                    chunk_index: 0,
                    control_nonce: 0,
                }),
                progress: AtomicU64::new(0),
                active: AtomicBool::new(true),
                started: AtomicBool::new(false),
            }),
        }
    }

    /// The stream id (stringified `progressToken`).
    pub fn progress_token(&self) -> &str {
        &self.inner.progress_token
    }

    /// Whether the writer is still live (not yet closed/aborted).
    ///
    /// `true` for any freshly-created writer; see [`has_started`](Self::has_started).
    pub fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// Whether the writer has begun streaming by publishing its `start` frame.
    ///
    /// Distinct from [`is_active`](Self::is_active): used to tell apart writers a
    /// tool actually streams through from ones created only because the request
    /// carried a progress token (the response-deferral guard).
    pub fn has_started(&self) -> bool {
        self.inner.started.load(Ordering::SeqCst)
    }

    /// Mint the next monotonic outer `progress` value (1, 2, 3, …).
    fn next_progress(&self) -> u64 {
        self.inner.progress.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Publish the lazy `start` frame on first use. Caller MUST hold the op lock.
    /// On a publish failure `started` stays `false`, so a later op retries.
    async fn start_internal(&self) -> crate::Result<()> {
        if self.inner.started.load(Ordering::SeqCst) || !self.inner.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        let notification = OpenStreamFrame::Start {
            content_type: self.inner.content_type.clone(),
        }
        .into_progress_notification(
            &self.inner.progress_token,
            self.next_progress(),
            None,
        )?;
        (self.inner.publish_frame)(notification).await?;
        self.inner.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Explicitly publish the `start` frame (idempotent; lazy on first `write`).
    pub async fn start(&self) -> crate::Result<()> {
        let _op = self.inner.op.lock().await;
        self.start_internal().await
    }

    /// Publish one ordered `chunk` frame, starting the stream lazily.
    pub async fn write(&self, data: String) -> crate::Result<()> {
        let mut op = self.inner.op.lock().await;
        self.start_internal().await?;
        if !self.inner.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        let progress = self.next_progress();
        let chunk_index = op.chunk_index;
        let notification = OpenStreamFrame::Chunk { chunk_index, data }
            .into_progress_notification(&self.inner.progress_token, progress, None)?;
        (self.inner.publish_frame)(notification).await?;
        op.chunk_index += 1;
        Ok(())
    }

    /// Publish a keepalive `ping` carrying a fresh `{token}:{n}` nonce.
    pub async fn ping(&self) -> crate::Result<()> {
        let mut op = self.inner.op.lock().await;
        if !self.inner.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        op.control_nonce += 1;
        let nonce = format!("{}:{}", self.inner.progress_token, op.control_nonce);
        let progress = self.next_progress();
        let notification = OpenStreamFrame::Ping { nonce }.into_progress_notification(
            &self.inner.progress_token,
            progress,
            None,
        )?;
        (self.inner.publish_frame)(notification).await?;
        Ok(())
    }

    /// Publish a `pong` echoing the peer's `ping` nonce.
    pub async fn pong(&self, nonce: String) -> crate::Result<()> {
        let _op = self.inner.op.lock().await;
        if !self.inner.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        let progress = self.next_progress();
        let notification = OpenStreamFrame::Pong { nonce }.into_progress_notification(
            &self.inner.progress_token,
            progress,
            None,
        )?;
        (self.inner.publish_frame)(notification).await?;
        Ok(())
    }

    /// Close the stream gracefully. Declares `lastChunkIndex` iff any chunks were
    /// written. Runs [`on_close`](OpenStreamWriterOptions::on_close) **after** the
    /// frame is published (even on publish failure); propagates the publish error.
    pub async fn close(&self) -> crate::Result<()> {
        let op = self.inner.op.lock().await;
        self.start_internal().await?;
        // Claim the terminal transition atomically; lose to a racing abort.
        if !self.inner.active.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let last_chunk_index = if op.chunk_index > 0 {
            Some(op.chunk_index - 1)
        } else {
            None
        };
        let notification = OpenStreamFrame::Close { last_chunk_index }.into_progress_notification(
            &self.inner.progress_token,
            self.next_progress(),
            None,
        )?;
        let publish_result = (self.inner.publish_frame)(notification).await;
        if let Some(hook) = &self.inner.on_close {
            hook().await;
        }
        publish_result.map(|_| ())
    }

    /// Abort the stream (terminal). Claims the terminal transition without the op
    /// lock, so it never waits on a stuck `write`. Runs
    /// [`on_abort`](OpenStreamWriterOptions::on_abort) **after** the frame is
    /// published (even on publish failure); propagates the publish error.
    /// Idempotent: a second `abort` (or one after `close`) is a no-op.
    pub async fn abort(&self, reason: Option<String>) -> crate::Result<()> {
        // Claim the terminal transition; no op lock → never blocks on a write.
        if !self.inner.active.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let notification = OpenStreamFrame::Abort {
            reason: reason.clone(),
        }
        .into_progress_notification(
            &self.inner.progress_token,
            self.next_progress(),
            None,
        )?;
        let publish_result = (self.inner.publish_frame)(notification).await;
        if let Some(hook) = &self.inner.on_abort {
            hook(reason).await;
        }
        publish_result.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::JsonRpcNotification;
    use crate::Error;
    use nostr_sdk::prelude::EventId;
    use std::sync::Mutex as StdMutex;

    type FrameLog = Arc<StdMutex<Vec<JsonRpcNotification>>>;

    fn frame_log() -> FrameLog {
        Arc::new(StdMutex::new(Vec::new()))
    }

    /// A publish closure that records every frame and returns a dummy id.
    fn recording_publisher(log: FrameLog) -> PublishFrame {
        Arc::new(move |frame: JsonRpcNotification| {
            let log = log.clone();
            Box::pin(async move {
                log.lock().unwrap().push(frame);
                Ok(EventId::all_zeros())
            })
        })
    }

    fn frame_type(notification: &JsonRpcNotification) -> String {
        notification.params.as_ref().unwrap()["cvm"]["frameType"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn frame_types(log: &FrameLog) -> Vec<String> {
        log.lock().unwrap().iter().map(frame_type).collect()
    }

    fn progress_of(notification: &JsonRpcNotification) -> u64 {
        notification.params.as_ref().unwrap()["progress"]
            .as_u64()
            .unwrap()
    }

    fn cvm(notification: &JsonRpcNotification) -> &serde_json::Value {
        &notification.params.as_ref().unwrap()["cvm"]
    }

    fn writer_with(log: FrameLog) -> OpenStreamWriter {
        OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "tok".to_string(),
            publish_frame: recording_publisher(log),
            content_type: None,
            on_close: None,
            on_abort: None,
        })
    }

    #[tokio::test]
    async fn has_started_reflects_start_or_chunk_frame() {
        let log = frame_log();
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-started".to_string(),
            publish_frame: recording_publisher(log),
            content_type: None,
            on_close: None,
            on_abort: None,
        });

        assert!(writer.is_active());
        assert!(!writer.has_started());

        // Control frames do not start the stream.
        writer.ping().await.unwrap();
        writer.pong("nonce".to_string()).await.unwrap();
        assert!(!writer.has_started());

        writer.write("hello".to_string()).await.unwrap();
        assert!(writer.has_started());
    }

    #[tokio::test]
    async fn has_started_after_explicit_start() {
        let log = frame_log();
        let writer = writer_with(log.clone());
        assert!(!writer.has_started());

        writer.start().await.unwrap();

        assert!(writer.has_started());
        assert_eq!(frame_types(&log), vec!["start"]);
    }

    #[tokio::test]
    async fn emits_ping_and_pong_with_matching_nonces() {
        let log = frame_log();
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-keepalive".to_string(),
            publish_frame: recording_publisher(log.clone()),
            content_type: None,
            on_close: None,
            on_abort: None,
        });

        writer.start().await.unwrap();
        writer.ping().await.unwrap();
        writer.pong("keepalive-nonce".to_string()).await.unwrap();

        let frames = log.lock().unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(progress_of(&frames[1]), 2);
        assert_eq!(cvm(&frames[1])["frameType"], "ping");
        assert_eq!(cvm(&frames[1])["nonce"], "token-keepalive:1");
        assert_eq!(progress_of(&frames[2]), 3);
        assert_eq!(cvm(&frames[2])["frameType"], "pong");
        assert_eq!(cvm(&frames[2])["nonce"], "keepalive-nonce");
    }

    #[tokio::test]
    async fn close_omits_last_chunk_index_when_no_chunks() {
        let log = frame_log();
        let writer = writer_with(log.clone());
        writer.close().await.unwrap();

        let frames = log.lock().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frame_type(&frames[1]), "close");
        assert!(!cvm(&frames[1])
            .as_object()
            .unwrap()
            .contains_key("lastChunkIndex"));
    }

    #[tokio::test]
    async fn close_includes_last_chunk_index_after_chunks() {
        let log = frame_log();
        let writer = writer_with(log.clone());
        writer.write("hello".to_string()).await.unwrap();
        writer.write("world".to_string()).await.unwrap();
        writer.close().await.unwrap();

        let frames = log.lock().unwrap();
        assert_eq!(frames.len(), 4);
        assert_eq!(frame_type(&frames[3]), "close");
        assert_eq!(cvm(&frames[3])["lastChunkIndex"], 1);
    }

    #[tokio::test]
    async fn lifecycle_hooks_fire_after_terminal_frames() {
        let lifecycle = Arc::new(StdMutex::new(Vec::<String>::new()));
        let log = frame_log();

        let lc = lifecycle.clone();
        let on_close: OnCloseHook = Arc::new(move || {
            let lc = lc.clone();
            Box::pin(async move {
                lc.lock().unwrap().push("close".to_string());
            })
        });
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-hooks".to_string(),
            publish_frame: recording_publisher(log.clone()),
            content_type: None,
            on_close: Some(on_close),
            on_abort: None,
        });
        writer.close().await.unwrap();
        assert_eq!(frame_type(log.lock().unwrap().last().unwrap()), "close");
        assert_eq!(*lifecycle.lock().unwrap(), vec!["close"]);

        let lc = lifecycle.clone();
        let on_abort: OnAbortHook = Arc::new(move |reason: Option<String>| {
            let lc = lc.clone();
            Box::pin(async move {
                lc.lock()
                    .unwrap()
                    .push(format!("abort:{}", reason.unwrap_or_default()));
            })
        });
        let abort_writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-hooks-abort".to_string(),
            publish_frame: recording_publisher(log.clone()),
            content_type: None,
            on_close: None,
            on_abort: Some(on_abort),
        });
        abort_writer.abort(Some("done".to_string())).await.unwrap();
        let frames = log.lock().unwrap();
        assert_eq!(frame_type(frames.last().unwrap()), "abort");
        assert_eq!(cvm(frames.last().unwrap())["reason"], "done");
        assert_eq!(*lifecycle.lock().unwrap(), vec!["close", "abort:done"]);
    }

    #[tokio::test]
    async fn publishes_abort_before_running_abort_hook() {
        let events = Arc::new(StdMutex::new(Vec::<String>::new()));

        let ev = events.clone();
        let publish: PublishFrame = Arc::new(move |frame: JsonRpcNotification| {
            let ev = ev.clone();
            Box::pin(async move {
                ev.lock()
                    .unwrap()
                    .push(format!("publish:{}", frame_type(&frame)));
                Ok(EventId::all_zeros())
            })
        });
        let ev = events.clone();
        let on_abort: OnAbortHook = Arc::new(move |reason: Option<String>| {
            let ev = ev.clone();
            Box::pin(async move {
                ev.lock()
                    .unwrap()
                    .push(format!("abort:{}", reason.unwrap_or_default()));
            })
        });
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-abort-order".to_string(),
            publish_frame: publish,
            content_type: None,
            on_close: None,
            on_abort: Some(on_abort),
        });

        writer.abort(Some("ordered".to_string())).await.unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["publish:abort", "abort:ordered"]
        );
    }

    #[tokio::test]
    async fn retries_start_when_first_start_publish_fails() {
        let log = frame_log();
        let fail_start = Arc::new(AtomicBool::new(true));

        let f = log.clone();
        let fs = fail_start.clone();
        let publish: PublishFrame = Arc::new(move |frame: JsonRpcNotification| {
            let f = f.clone();
            let fs = fs.clone();
            Box::pin(async move {
                if frame_type(&frame) == "start" && fs.swap(false, Ordering::SeqCst) {
                    return Err(Error::Transport("relay unavailable".to_string()));
                }
                f.lock().unwrap().push(frame);
                Ok(EventId::all_zeros())
            })
        });
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-start-retry".to_string(),
            publish_frame: publish,
            content_type: None,
            on_close: None,
            on_abort: None,
        });

        assert!(writer.start().await.is_err());
        writer.write("hello".to_string()).await.unwrap();
        assert_eq!(frame_types(&log), vec!["start", "chunk"]);
    }

    #[tokio::test]
    async fn runs_close_cleanup_when_close_publish_fails() {
        let lifecycle = Arc::new(StdMutex::new(Vec::<String>::new()));
        let publish: PublishFrame = Arc::new(move |frame: JsonRpcNotification| {
            Box::pin(async move {
                if frame_type(&frame) == "close" {
                    return Err(Error::Transport("close publish failed".to_string()));
                }
                Ok(EventId::all_zeros())
            })
        });
        let lc = lifecycle.clone();
        let on_close: OnCloseHook = Arc::new(move || {
            let lc = lc.clone();
            Box::pin(async move {
                lc.lock().unwrap().push("close".to_string());
            })
        });
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-close-fail".to_string(),
            publish_frame: publish,
            content_type: None,
            on_close: Some(on_close),
            on_abort: None,
        });

        assert!(writer.close().await.is_err());
        assert!(!writer.is_active());
        assert_eq!(*lifecycle.lock().unwrap(), vec!["close"]);
    }

    #[tokio::test]
    async fn runs_abort_cleanup_when_abort_publish_fails() {
        let lifecycle = Arc::new(StdMutex::new(Vec::<String>::new()));
        let publish: PublishFrame = Arc::new(move |frame: JsonRpcNotification| {
            Box::pin(async move {
                if frame_type(&frame) == "abort" {
                    return Err(Error::Transport("abort publish failed".to_string()));
                }
                Ok(EventId::all_zeros())
            })
        });
        let lc = lifecycle.clone();
        let on_abort: OnAbortHook = Arc::new(move |reason: Option<String>| {
            let lc = lc.clone();
            Box::pin(async move {
                lc.lock()
                    .unwrap()
                    .push(format!("abort:{}", reason.unwrap_or_default()));
            })
        });
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-abort-fail".to_string(),
            publish_frame: publish,
            content_type: None,
            on_close: None,
            on_abort: Some(on_abort),
        });

        assert!(writer.abort(Some("cleanup".to_string())).await.is_err());
        assert!(!writer.is_active());
        assert_eq!(*lifecycle.lock().unwrap(), vec!["abort:cleanup"]);
    }

    #[tokio::test]
    async fn abort_deactivates_without_waiting_for_a_stuck_write() {
        let lifecycle = Arc::new(StdMutex::new(Vec::<String>::new()));
        let reached = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());

        let r = reached.clone();
        let g = gate.clone();
        let publish: PublishFrame = Arc::new(move |frame: JsonRpcNotification| {
            let r = r.clone();
            let g = g.clone();
            Box::pin(async move {
                if frame_type(&frame) == "chunk" {
                    // Signal that the write has reached the (stuck) chunk publish,
                    // then block until released.
                    r.notify_one();
                    g.notified().await;
                }
                Ok(EventId::all_zeros())
            })
        });
        let lc = lifecycle.clone();
        let on_abort: OnAbortHook = Arc::new(move |reason: Option<String>| {
            let lc = lc.clone();
            Box::pin(async move {
                lc.lock()
                    .unwrap()
                    .push(format!("abort:{}", reason.unwrap_or_default()));
            })
        });
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-stuck".to_string(),
            publish_frame: publish,
            content_type: None,
            on_close: None,
            on_abort: Some(on_abort),
        });

        let writer2 = writer.clone();
        let write_handle = tokio::spawn(async move { writer2.write("hello".to_string()).await });

        // Wait until the write is parked inside the stuck chunk publish.
        reached.notified().await;

        // Abort must complete without acquiring the op lock the stuck write holds.
        writer
            .abort(Some("stuck publish".to_string()))
            .await
            .unwrap();
        assert!(!writer.is_active());
        assert_eq!(*lifecycle.lock().unwrap(), vec!["abort:stuck publish"]);

        // Release the stuck write so the spawned task can finish cleanly.
        gate.notify_one();
        write_handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn serializes_concurrent_writes_before_close() {
        let log = frame_log();
        let f = log.clone();
        let publish: PublishFrame = Arc::new(move |frame: JsonRpcNotification| {
            let f = f.clone();
            Box::pin(async move {
                // A small delay on chunks would expose any interleaving if the op
                // lock did not serialize build+publish.
                if frame_type(&frame) == "chunk" {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                f.lock().unwrap().push(frame);
                Ok(EventId::all_zeros())
            })
        });
        let writer = OpenStreamWriter::new(OpenStreamWriterOptions {
            progress_token: "token-concurrent".to_string(),
            publish_frame: publish,
            content_type: None,
            on_close: None,
            on_abort: None,
        });

        let (a, b, c) = tokio::join!(
            writer.write("hello".to_string()),
            writer.write("world".to_string()),
            writer.close()
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();

        let frames = log.lock().unwrap();
        let types: Vec<String> = frames.iter().map(frame_type).collect();
        assert_eq!(types, vec!["start", "chunk", "chunk", "close"]);
        assert_eq!(progress_of(&frames[1]), 2);
        assert_eq!(cvm(&frames[1])["chunkIndex"], 0);
        assert_eq!(cvm(&frames[1])["data"], "hello");
        assert_eq!(progress_of(&frames[2]), 3);
        assert_eq!(cvm(&frames[2])["chunkIndex"], 1);
        assert_eq!(cvm(&frames[2])["data"], "world");
        assert_eq!(progress_of(&frames[3]), 4);
        assert_eq!(cvm(&frames[3])["lastChunkIndex"], 1);
    }
}
