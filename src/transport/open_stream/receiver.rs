//! Thin transport-facing adapter over an [`OpenStreamRegistry`].
//!
//! Ports `sdk/src/transport/open-stream/receiver.ts`. The client transport owns
//! one receiver (single peer); the server owns one per peer. It recognizes
//! inbound CEP-41 frames
//! ([`is_open_stream_frame`](OpenStreamReceiver::is_open_stream_frame)) and feeds
//! them to the registry with the real clock
//! ([`process_frame`](OpenStreamReceiver::process_frame)). The registry's own
//! `process_frame` takes an explicit `now` so the engine stays unit-testable
//! with an injected clock; this adapter supplies `Instant::now()` at the
//! transport boundary.

use std::time::Instant;

use crate::core::types::JsonRpcNotification;

use super::errors::OpenStreamError;
use super::registry::{OpenStreamRegistry, OpenStreamRegistryPolicy};
use super::session::{FrameOutcome, OpenStreamSession};

/// Transport-facing CEP-41 receiver wrapping a per-peer registry.
pub struct OpenStreamReceiver {
    registry: OpenStreamRegistry,
}

impl Default for OpenStreamReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenStreamReceiver {
    /// Create a receiver with the default policy.
    pub fn new() -> Self {
        Self {
            registry: OpenStreamRegistry::new(),
        }
    }

    /// Create a receiver with an explicit registry policy.
    pub fn with_policy(policy: OpenStreamRegistryPolicy) -> Self {
        Self {
            registry: OpenStreamRegistry::with_policy(policy),
        }
    }

    /// Returns `true` when `notification` carries a CEP-41 frame in `params.cvm`.
    pub fn is_open_stream_frame(notification: &JsonRpcNotification) -> bool {
        OpenStreamRegistry::is_open_stream_progress(notification)
    }

    /// Feed one inbound `notifications/progress` frame to the registry, stamping
    /// liveness with the real clock.
    pub async fn process_frame(
        &mut self,
        notification: &JsonRpcNotification,
    ) -> Result<FrameOutcome, OpenStreamError> {
        self.registry
            .process_frame(Instant::now(), notification)
            .await
    }

    /// Look up an active reader session by token.
    pub fn get_session(&self, progress_token: &str) -> Option<OpenStreamSession> {
        self.registry.get_session(progress_token)
    }

    /// Number of active streams.
    pub fn active_stream_count(&self) -> usize {
        self.registry.size()
    }

    /// Borrow the underlying registry (e.g. to create an outbound reader session
    /// or drive the keepalive sweep).
    pub fn registry(&self) -> &OpenStreamRegistry {
        &self.registry
    }

    /// Mutably borrow the underlying registry.
    pub fn registry_mut(&mut self) -> &mut OpenStreamRegistry {
        &mut self.registry
    }

    /// Dispose all sessions and clear the registry.
    pub fn clear(&mut self) {
        self.registry.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::open_stream::frame::OpenStreamFrame;
    use serde_json::json;

    fn start_notification(token: &str, progress: u64) -> JsonRpcNotification {
        OpenStreamFrame::Start { content_type: None }
            .into_progress_notification(token, progress, None)
            .unwrap()
    }

    #[test]
    fn is_open_stream_frame_detects_cvm_payload() {
        let frame = start_notification("tok", 1);
        assert!(OpenStreamReceiver::is_open_stream_frame(&frame));

        let plain = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({ "progressToken": "tok", "progress": 1 })),
        };
        assert!(!OpenStreamReceiver::is_open_stream_frame(&plain));

        // An oversized-transfer frame is not an open-stream frame.
        let oversized = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({
                "progressToken": "tok",
                "progress": 1,
                "cvm": { "type": "oversized-transfer", "frameType": "end" }
            })),
        };
        assert!(!OpenStreamReceiver::is_open_stream_frame(&oversized));
    }

    #[tokio::test]
    async fn process_frame_delegates_to_registry_and_tracks_session() {
        let mut receiver = OpenStreamReceiver::new();
        receiver
            .process_frame(&start_notification("tok", 1))
            .await
            .unwrap();
        assert_eq!(receiver.active_stream_count(), 1);
        assert!(receiver.get_session("tok").is_some());

        // Driving a graceful close through the adapter removes the session.
        let close = OpenStreamFrame::Close {
            last_chunk_index: None,
        }
        .into_progress_notification("tok", 2, None)
        .unwrap();
        receiver.process_frame(&close).await.unwrap();
        assert_eq!(receiver.active_stream_count(), 0);
    }

    #[tokio::test]
    async fn non_open_stream_progress_notification_is_not_intercepted() {
        let mut receiver = OpenStreamReceiver::new();

        // A CEP-22 oversized-transfer frame and a plain progress notification are
        // both NOT open-stream frames — the two receivers are type-disjoint.
        let oversized = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({
                "progressToken": "tok",
                "progress": 1,
                "cvm": { "type": "oversized-transfer", "frameType": "end" }
            })),
        };
        let plain = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/progress".to_string(),
            params: Some(json!({ "progressToken": "tok", "progress": 1 })),
        };

        for notification in [&oversized, &plain] {
            assert!(!OpenStreamReceiver::is_open_stream_frame(notification));
            // The dispatcher only feeds the registry when the predicate is true.
            // Mirror that gate: a non-open-stream frame must never be processed.
            if OpenStreamReceiver::is_open_stream_frame(notification) {
                receiver.process_frame(notification).await.unwrap();
            }
        }

        // Neither frame reached the registry — no session was created/tracked.
        assert_eq!(receiver.active_stream_count(), 0);
        assert!(receiver.get_session("tok").is_none());
    }
}
