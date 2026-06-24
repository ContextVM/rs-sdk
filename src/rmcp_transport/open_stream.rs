//! CEP-41: the ergonomic `call_tool_stream` consumer API.
//!
//! Ports `sdk/src/transport/call-tool-stream.ts`. Pairs a normal rmcp
//! `tools/call` with its CEP-41 open stream: the call is issued with
//! progress-aware request options (so rmcp stamps a `progressToken` and arms the
//! reset-on-progress watcher), and the transport binds the SDK-stamped token to a
//! reader [`OpenStreamSession`] the moment the request is published (via the
//! [`ClientOpenStreamHandle`] obtained before the transport is served).
//!
//! One `tools/call` yields two outputs: the live chunk [`stream`](ToolStreamCall::stream)
//! and the eventual final [`result`](ToolStreamCall::result) — exactly the CEP-41
//! supplement-not-replace semantics.

use std::time::Duration;

use futures::future::BoxFuture;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{Peer, PeerRequestOptions, ServiceError};
use rmcp::RoleClient;
use tokio::task::{JoinError, JoinHandle};

use crate::core::error::{Error, Result};
use crate::transport::client::ClientOpenStreamHandle;
use crate::transport::open_stream::OpenStreamSession;

use super::progress::PeerRequestOptionsExt;

type AbortFn = Box<dyn Fn(Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;

/// A live CEP-41 tool call: the incremental chunk [`stream`](Self::stream), the
/// eventual final [`result`](Self::result), and an [`abort`](Self::abort) handle.
pub struct ToolStreamCall {
    /// The stringified `progressToken` correlating the call and its stream.
    pub progress_token: String,
    /// The async stream of payload chunks (`impl Stream<Item = Result<String, OpenStreamError>>`).
    pub stream: OpenStreamSession,
    /// The final `CallToolResult`, resolving after the stream closes (deferral).
    ///
    /// A **flat** result: the spawned-task (`JoinError`) and rmcp (`ServiceError`)
    /// failures are folded into [`crate::Error`], so consumers `await` once rather
    /// than unwrapping a nested `Result`.
    pub result: BoxFuture<'static, Result<CallToolResult>>,
    abort_fn: AbortFn,
}

impl ToolStreamCall {
    /// Consumer cancel: publish an `abort` frame to the server (so its writer
    /// aborts), finalize the local stream, and free the reader registry slot.
    pub async fn abort(&self, reason: Option<String>) {
        (self.abort_fn)(reason).await;
    }
}

impl std::fmt::Debug for ToolStreamCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolStreamCall")
            .field("progress_token", &self.progress_token)
            .finish_non_exhaustive()
    }
}

/// Build the progress-aware [`PeerRequestOptions`] for an open-stream call.
///
/// Idle timeout covers a full keepalive cycle (idle → probe → close-grace) so the
/// rmcp request is never failed before the open-stream keepalive would have
/// aborted a genuinely-dead stream; `reset_timeout_on_progress` re-arms it on
/// every forwarded chunk/keepalive frame. A hard lifetime cap is applied **only**
/// when `max_total_timeout_ms` is set — never the CEP-22 oversized default (an
/// open stream may legitimately run unbounded).
fn open_stream_request_options(handle: &ClientOpenStreamHandle) -> PeerRequestOptions {
    let config = handle.config();
    let idle_ms = config
        .idle_timeout_ms
        .saturating_add(config.probe_timeout_ms)
        .saturating_add(config.close_grace_period_ms)
        .max(1);
    let mut options = PeerRequestOptions::with_timeout(Duration::from_millis(idle_ms))
        .reset_timeout_on_progress();
    if let Some(max_total_ms) = config.max_total_timeout_ms {
        options = options.with_max_total_timeout(Duration::from_millis(max_total_ms));
    }
    options
}

/// Call an MCP tool and return the paired CEP-41 [`ToolStreamCall`].
///
/// The reader session is registered **before** the request is published (the
/// placeholder is bound synchronously inside the transport's `send`), so no
/// inbound chunk can race ahead of it. The call itself runs on a spawned task so
/// this returns as soon as the stream handle is available — long before the final
/// result settles.
pub async fn call_tool_stream(
    peer: &Peer<RoleClient>,
    transport: &ClientOpenStreamHandle,
    params: CallToolRequestParams,
) -> Result<ToolStreamCall> {
    // Serialize the placeholder push→bind window so two concurrent
    // `call_tool_stream` calls cannot cross their FIFO placeholders against the
    // tokens rmcp stamps (the transport binds by FIFO order, but rmcp/worker order
    // is independent). At most one placeholder is ever unbound while this is held;
    // it is released the moment this call's session binds, so the streams
    // themselves still run fully concurrently.
    let bind_guard = transport.bind_lock().clone().lock_owned().await;

    // 1. Register the placeholder for the reader session (resolved by `send`).
    let pending = transport.prepare_outbound();

    // 2. Build progress-aware options (rmcp stamps the token + arms the watcher).
    let options = open_stream_request_options(transport);

    // 3. Issue the call on a spawned task so we can hand back the stream first.
    let peer = peer.clone();
    let mut result_handle: JoinHandle<std::result::Result<CallToolResult, ServiceError>> =
        tokio::spawn(async move { peer.call_tool_with_options(params, options).await });

    // 4. Bind the reader session. Race `pending` against the call itself: if the
    //    call settles BEFORE the transport binds the placeholder (e.g. rmcp
    //    rejects the request before publishing), `pending` would never resolve and
    //    the bind lock would deadlock every later `call_tool_stream`. `biased`
    //    prefers the bind (a successful call always publishes — and binds — first,
    //    so the call-settled arm fires only on a pre-publish failure).
    let (progress_token, stream) = tokio::select! {
        biased;
        bound = pending => match bound {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                drop(bind_guard);
                result_handle.abort();
                return Err(error);
            }
            Err(_) => {
                // The transport closed (or dropped the placeholder) before binding.
                drop(bind_guard);
                result_handle.abort();
                return Err(Error::Transport(
                    "transport closed before the outbound open-stream session was bound"
                        .to_string(),
                ));
            }
        },
        settled = &mut result_handle => {
            // The call finished without ever binding a session — drop the orphaned
            // placeholder so the next `tools/call` is not mis-bound to it, then
            // surface the (flattened) error.
            transport.cancel_outbound();
            drop(bind_guard);
            return Err(match flatten_call_result(settled) {
                Err(error) => error,
                Ok(_) => Error::Other(
                    "open-stream tool call completed without establishing a stream".to_string(),
                ),
            });
        }
    };
    drop(bind_guard);

    // 5. Build the abort handle (publish abort + finalize + free the slot).
    let registry = transport.registry();
    let abort_session = stream.clone();
    let abort_token = progress_token.clone();
    let abort_fn: AbortFn = Box::new(move |reason: Option<String>| {
        let registry = registry.clone();
        let session = abort_session.clone();
        let token = abort_token.clone();
        Box::pin(async move {
            // Publish the `abort` frame to the server + finalize locally.
            session.abort(reason.clone()).await;
            // Free the concurrency slot + run any hook (idempotent re-finalize).
            registry.lock().await.consumer_abort(&token, reason).await;
        })
    });

    // 6. Flatten the spawned call into a single-`await` future (JoinError +
    //    ServiceError folded into `crate::Error`).
    let result: BoxFuture<'static, Result<CallToolResult>> =
        Box::pin(async move { flatten_call_result(result_handle.await) });

    Ok(ToolStreamCall {
        progress_token,
        stream,
        result,
        abort_fn,
    })
}

/// Fold the doubly-nested `call_tool_with_options` outcome (`JoinError` outside,
/// `ServiceError` inside) into a flat [`crate::Error`].
fn flatten_call_result(
    settled: std::result::Result<std::result::Result<CallToolResult, ServiceError>, JoinError>,
) -> Result<CallToolResult> {
    match settled {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(service_error)) => Err(Error::Transport(service_error.to_string())),
        Err(join_error) => Err(Error::Other(format!(
            "call_tool_stream task failed: {join_error}"
        ))),
    }
}
