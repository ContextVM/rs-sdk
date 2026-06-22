//! CEP-22: progress-aware request options for rmcp consumers.
//!
//! Under the rmcp fork, plain [`Peer`] calls such as `call_tool` use
//! `PeerRequestOptions::no_options()` — **no timeout at all**: a stalled
//! oversized response hangs the caller forever. This module closes that gap
//! the way the TS SDK's docs do — by passing request options on the existing
//! call — via an extension trait carrying an options-taking `call_tool`
//! variant, plus a constructor for the recommended progress-aware settings.
//!
//! With CEP-22 enabled, the Nostr transports forward each inbound transfer
//! frame to the requester as a plain progress notification, so an idle
//! timeout built by [`progress_aware_options`] resets on every chunk: a live
//! transfer can run long, a stalled one fails after `idle`, and
//! `max_total` caps the call regardless of progress.
//!
//! ```ignore
//! use contextvm_sdk::{progress_aware_options, PeerRequestOptionsExt};
//!
//! let result = running_service
//!     .peer()
//!     .call_tool_with_options(
//!         params,
//!         progress_aware_options(
//!             contextvm_sdk::DEFAULT_OVERSIZED_IDLE_TIMEOUT,
//!             contextvm_sdk::DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT,
//!         ),
//!     )
//!     .await?;
//! ```

use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, ServiceError};
use rmcp::RoleClient;

/// Default requester-side idle timeout for requests that may trigger a CEP-22
/// oversized transfer: 60 s.
///
/// TS parity twice over: equals the upstream TS SDK's blanket per-request
/// timeout (`DEFAULT_REQUEST_TIMEOUT_MSEC`), and exceeds the worst-case
/// inter-chunk gap including the 30 s accept wait
/// ([`DEFAULT_ACCEPT_TIMEOUT_MS`](crate::transport::oversized_transfer::DEFAULT_ACCEPT_TIMEOUT_MS)).
pub const DEFAULT_OVERSIZED_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Default requester-side max-total timeout: 300 s.
///
/// Aligned with the receiver-side watchdog default
/// ([`DEFAULT_TRANSFER_TIMEOUT_MS`](crate::transport::oversized_transfer::DEFAULT_TRANSFER_TIMEOUT_MS))
/// so the requester gives up in the same window the receiver reaps state —
/// symmetric failure. (Upstream TS sets no max-total default; here it stays
/// opt-in via [`progress_aware_options`], never baked into plain calls.)
pub const DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Build the recommended [`PeerRequestOptions`] for requests whose responses
/// may arrive as CEP-22 oversized transfers: an `idle` timeout that resets on
/// every progress notification, capped by `max_total`.
///
/// Equivalent to
/// `PeerRequestOptions::with_timeout(idle).reset_timeout_on_progress().with_max_total_timeout(max_total)`.
/// Named after the mechanism (progress-aware timeouts), not the oversized use
/// case — mirroring the TS SDK, where "oversized" appears in docs but never in
/// the API surface.
///
/// Sizing notes:
/// - `reset_timeout_on_progress` without an idle timeout is a **no-op** — the
///   fork registers a progress watcher only when *both* are set, which is why
///   this constructor takes `idle` rather than making it optional.
/// - The client→server upload direction receives at most **one** inbound
///   reset (the server's `accept` handshake frame) — and it reaches the rmcp
///   service loop only after the whole upload send returns. Size `idle` and
///   `max_total` to cover the full upload duration, not just the gaps.
/// - Sensible defaults: [`DEFAULT_OVERSIZED_IDLE_TIMEOUT`] /
///   [`DEFAULT_OVERSIZED_MAX_TOTAL_TIMEOUT`]. They intentionally mirror the
///   transport's `OversizedTransferConfig` numbers but are not read from it —
///   the peer layer has no access to transport config by design; align them
///   manually if you tune the transport.
pub fn progress_aware_options(idle: Duration, max_total: Duration) -> PeerRequestOptions {
    PeerRequestOptions::with_timeout(idle)
        .reset_timeout_on_progress()
        .with_max_total_timeout(max_total)
}

/// Options-taking call variants for [`Peer<RoleClient>`] — the rs-side analog
/// of passing `RequestOptions` inline to the TS SDK's `client.callTool`.
///
/// Without these, high-level fork calls (`peer.call_tool(..)` etc.) run with
/// `PeerRequestOptions::no_options()`: **no timeout, infinite await**. Pair
/// with [`progress_aware_options`] for any call whose response may be large
/// (CEP-22 fragments every rmcp request's response once the peer advertises
/// support — rmcp stamps a progress token into every outgoing request).
///
/// For request types without a dedicated variant here, the generic path is
/// two lines on public fork API — no wrapper needed:
///
/// ```ignore
/// let handle = peer.send_cancellable_request(request, options).await?;
/// let response = handle.await_response().await?;
/// ```
///
/// On timeout the call fails with `ServiceError::Timeout { timeout }` (the
/// value identifies which timer fired: `idle` vs `max_total`) and rmcp
/// publishes a `notifications/cancelled` for the request.
pub trait PeerRequestOptionsExt {
    /// `call_tool` with explicit [`PeerRequestOptions`] — the direct analog of
    /// TS `client.callTool(params, schema, options)`.
    fn call_tool_with_options(
        &self,
        params: CallToolRequestParams,
        options: PeerRequestOptions,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ServiceError>> + Send;
}

impl PeerRequestOptionsExt for Peer<RoleClient> {
    async fn call_tool_with_options(
        &self,
        params: CallToolRequestParams,
        options: PeerRequestOptions,
    ) -> Result<CallToolResult, ServiceError> {
        // Mirrors the fork's `method!` expansion for `call_tool`
        // (service/client.rs), with options threaded through
        // `send_cancellable_request` instead of the option-less default.
        let result = self
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                options,
            )
            .await?
            .await_response()
            .await?;
        match result {
            ServerResult::CallToolResult(result) => Ok(result),
            _ => Err(ServiceError::UnexpectedResponse),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the `progress_aware_options` wiring: the constructor must (a) set the
    /// idle `timeout`, (b) enable `reset_timeout_on_progress` — without which the
    /// fork registers no progress watcher (`send_request_with_option` only arms
    /// one when *both* the flag and a timeout are set), so inbound chunks would
    /// never reset the timer — and (c) populate `max_total_timeout`. Distinct
    /// idle/max-total values also catch an accidental argument swap in the
    /// builder chain. End-to-end behavior is covered by the timeout tests in
    /// `tests/oversized_timeout_e2e.rs`; this is the fast unit guard on the flags.
    #[test]
    fn progress_aware_options_sets_reset_and_both_timeouts() {
        let idle = Duration::from_millis(250);
        let max_total = Duration::from_secs(42);

        let options = progress_aware_options(idle, max_total);

        assert_eq!(options.timeout, Some(idle), "idle timeout must be set");
        assert!(
            options.reset_timeout_on_progress,
            "reset_timeout_on_progress must be enabled or the fork arms no progress watcher"
        );
        assert_eq!(
            options.max_total_timeout,
            Some(max_total),
            "max_total_timeout must be set"
        );
    }
}
