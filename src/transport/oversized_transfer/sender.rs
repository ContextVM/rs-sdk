//! Transport-agnostic async driver for sending an oversized transfer.
//!
//! Given the ordered frames produced by [`build_oversized_frames`](super::codec::build_oversized_frames),
//! this sequences `start → [await accept] → chunks → end`, publishing each frame
//! through a caller-supplied closure, and returns the **end-frame `EventId`** so
//! the transport can correlate the eventual response.
//!
//! The driver carries no transport itself: the client publishes via
//! `prepare_mcp_message` + `publish_event` (registering a pending entry between
//! the two), the server via `base.send_mcp_message`. Those signatures differ, so
//! the publish step is injected as a closure and this module stays transport-free
//! (there is no active `abort` frame in v1 — on a missed `accept` the sender
//! fails and cleans up locally, letting the peer's own timeout fire).

use std::future::Future;
use std::time::Duration;

use nostr_sdk::prelude::EventId;
use tokio::sync::oneshot;

use crate::core::types::JsonRpcNotification;

use super::codec::BuiltOversizedFrames;
use super::errors::OversizedTransferError;

/// Extract the outer `progressToken` from a frame's `params`, used to label a
/// locally-raised [`OversizedTransferError::Abort`].
fn progress_token_of(frame: &JsonRpcNotification) -> String {
    frame
        .params
        .as_ref()
        .and_then(|params| params.get("progressToken"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Publish an oversized transfer in canonical frame order, returning the
/// `EventId` of the **end** frame (the id the peer correlates its response to).
///
/// `publish` is invoked once per frame, in order, and must return the published
/// event's inner `EventId`. When `needs_accept` is set the driver waits up to
/// `accept_timeout` for the receiver's `accept` to fire `await_accept` before
/// sending any chunk; on timeout (or a dropped waiter) it returns
/// [`OversizedTransferError::Abort`] without emitting an `abort` frame.
///
/// `await_accept` is `Some` iff `needs_accept` is `true`.
pub async fn send_oversized_transfer<P, Fut>(
    frames: BuiltOversizedFrames,
    needs_accept: bool,
    await_accept: Option<oneshot::Receiver<()>>,
    accept_timeout: Duration,
    mut publish: P,
) -> crate::Result<EventId>
where
    P: FnMut(JsonRpcNotification) -> Fut,
    Fut: Future<Output = crate::Result<EventId>>,
{
    // The token is only needed to label a potential abort; capture it before the
    // start frame is moved into `publish`.
    let token = progress_token_of(&frames.start);

    // 1. Publish `start`. Its id is not used for correlation — the end id is.
    publish(frames.start).await?;

    // 2. If a handshake is required, block until the receiver's `accept` arrives.
    if needs_accept {
        let outcome = match await_accept {
            Some(rx) => tokio::time::timeout(accept_timeout, rx).await,
            None => {
                // Defensive: `needs_accept` implies a waiter was provided.
                return Err(OversizedTransferError::abort(
                    token,
                    Some("no accept waiter registered".to_string()),
                )
                .into());
            }
        };

        // `Err(_)` → timed out; `Ok(Err(_))` → waiter dropped. Either way we abort
        // locally and let the peer's own timeout fire (no active abort frame).
        if matches!(outcome, Err(_) | Ok(Err(_))) {
            return Err(OversizedTransferError::abort(
                token,
                Some("timed out waiting for accept frame".to_string()),
            )
            .into());
        }
    }

    // 3. Publish each chunk sequentially in canonical (`progress`) order.
    for chunk in frames.chunks {
        publish(chunk).await?;
    }

    // 4. Publish `end`; its id correlates the eventual response.
    let end_id = publish(frames.end).await?;
    Ok(end_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::transport::oversized_transfer::codec::{
        build_oversized_frames, OversizedSenderOptions,
    };

    /// `frameType` discriminator of a published frame.
    fn frame_type(notification: &JsonRpcNotification) -> String {
        notification.params.as_ref().unwrap()["cvm"]["frameType"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Deterministic [`EventId`] for the n-th published frame (1-based).
    fn nth_event_id(n: usize) -> EventId {
        EventId::from_hex(&format!("{n:064x}")).unwrap()
    }

    /// Build frames for a payload large enough to span several chunks.
    fn sample_frames(token: &str, needs_accept: bool) -> BuiltOversizedFrames {
        let payload = "x".repeat(50);
        let options = OversizedSenderOptions::new(token)
            .with_chunk_size(8)
            .with_accept_handshake(needs_accept);
        build_oversized_frames(&payload, &options).unwrap()
    }

    /// An in-memory publish closure: records each frame and hands back a
    /// deterministic id keyed on publish order (matching [`nth_event_id`]).
    fn recording_publisher(
        log: Rc<RefCell<Vec<JsonRpcNotification>>>,
    ) -> impl FnMut(JsonRpcNotification) -> std::pin::Pin<Box<dyn Future<Output = crate::Result<EventId>>>>
    {
        move |frame: JsonRpcNotification| {
            let log = log.clone();
            Box::pin(async move {
                log.borrow_mut().push(frame);
                let n = log.borrow().len();
                Ok(nth_event_id(n))
            })
        }
    }

    #[tokio::test]
    async fn publishes_frames_in_canonical_order_and_returns_end_id() {
        let frames = sample_frames("tok", false);
        let expected_count = frames.frame_count();

        let log = Rc::new(RefCell::new(Vec::new()));
        let publish = recording_publisher(log.clone());

        let end_id = send_oversized_transfer(frames, false, None, Duration::from_secs(1), publish)
            .await
            .unwrap();

        let frames_log = log.borrow();
        assert_eq!(frames_log.len(), expected_count);
        assert!(expected_count > 2, "payload should span multiple chunks");

        // start → chunks… → end
        assert_eq!(frame_type(&frames_log[0]), "start");
        assert_eq!(frame_type(frames_log.last().unwrap()), "end");
        for mid in &frames_log[1..frames_log.len() - 1] {
            assert_eq!(frame_type(mid), "chunk");
        }

        // Returned id is the end frame's id (the last one published).
        assert_eq!(end_id, nth_event_id(expected_count));
    }

    #[tokio::test]
    async fn accept_is_awaited_and_satisfied_when_needed() {
        let frames = sample_frames("tok", true);
        let expected_count = frames.frame_count();

        let log = Rc::new(RefCell::new(Vec::new()));
        let publish = recording_publisher(log.clone());

        // Pre-arm the oneshot so the awaited accept resolves immediately.
        let (tx, rx) = oneshot::channel();
        tx.send(()).unwrap();

        let end_id =
            send_oversized_transfer(frames, true, Some(rx), Duration::from_secs(1), publish)
                .await
                .unwrap();

        let frames_log = log.borrow();
        assert_eq!(frames_log.len(), expected_count);
        assert_eq!(frame_type(&frames_log[0]), "start");
        assert_eq!(frame_type(frames_log.last().unwrap()), "end");
        assert_eq!(end_id, nth_event_id(expected_count));
    }

    #[tokio::test]
    async fn accept_is_not_awaited_when_not_needed() {
        let frames = sample_frames("tok", false);

        let log = Rc::new(RefCell::new(Vec::new()));
        let publish = recording_publisher(log.clone());

        // A waiter is supplied but never fired; because `needs_accept` is false
        // the driver must ignore it and complete without abort.
        let (_tx, rx) = oneshot::channel();
        let result =
            send_oversized_transfer(frames, false, Some(rx), Duration::from_millis(5), publish)
                .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn missed_accept_returns_abort() {
        let frames = sample_frames("tok", true);

        let log = Rc::new(RefCell::new(Vec::new()));
        let publish = recording_publisher(log.clone());

        // Keep `_tx` alive so the waiter is not "closed" — this exercises the
        // timeout (elapsed) path specifically.
        let (_tx, rx) = oneshot::channel();
        let result =
            send_oversized_transfer(frames, true, Some(rx), Duration::from_millis(20), publish)
                .await;

        match result {
            Err(crate::Error::OversizedTransfer(OversizedTransferError::Abort {
                token, ..
            })) => assert_eq!(token, "tok"),
            other => panic!("expected abort error, got {other:?}"),
        }

        // Only the start frame was published before the failed handshake.
        assert_eq!(log.borrow().len(), 1);
    }
}
