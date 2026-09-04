//! Inbound middleware seam for the server transport (rs-sdk port of TS `addInboundMiddleware`).
//!
//! An ordered chain of [`InboundMiddleware`] runs as the final inbound stage, just before a message
//! is forwarded to the rmcp worker. A middleware **forwards** by calling [`Next::run`] and **drops**
//! by returning without calling it. The chain is behavior-preserving until a middleware is
//! registered: with an empty chain, `dispatch_inbound` reproduces today's direct
//! `tx.send(IncomingRequest)`.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::future::FutureExt; // .catch_unwind()
use nostr_sdk::prelude::Event;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::{IncomingRequest, ServerEventRouteStore, ServerOpenStreamState, LOG_TARGET};
use crate::core::types::{JsonRpcMessage, PaymentInteractionMode};

/// Per-event context handed to every inbound middleware.
///
/// Built once per event and shared (by `Arc`) across the whole chain. The four transport-level
/// fields are populated at the seam. The two CEP-8 fields have different lifetimes:
/// `client_pmis` is read off this event's tags, while `payment_interaction` is the mode negotiated
/// for the session.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InboundContext {
    /// Client (author) pubkey, hex. Inner pubkey for gift-wrapped requests.
    pub client_pubkey: String,
    /// Nostr event id used for response correlation (the worker rewrites the request id to this).
    pub request_event_id: String,
    /// Whether the inbound event was encrypted.
    pub is_encrypted: bool,
    /// Inbound gift-wrap kind to mirror on the response (CEP-19); `None` for plaintext.
    pub mirrored_wrap_kind: Option<u16>,
    /// Payment method ids the client advertised **on this event** (CEP-8 `pmi` tags), in
    /// preference order. `None` when this event carried none, which says nothing about what the
    /// client advertised earlier: the ids are not accumulated on the session, so a client that
    /// sent them with `initialize` presents `None` on a later invocation.
    pub client_pmis: Option<Vec<String>>,
    /// Effective CEP-8 payment interaction mode negotiated for the session this event belongs to.
    /// Unlike [`client_pmis`](Self::client_pmis) this persists across the session, and an
    /// unnegotiated session resolves to [`PaymentInteractionMode::Transparent`] rather than `None`.
    pub payment_interaction: Option<PaymentInteractionMode>,
    /// Per-event cancellation token, a child of the transport's shutdown token.
    ///
    /// A middleware doing long-running work (e.g. a payment verification) should derive its own
    /// child from this and select on it, so closing the transport aborts the work instead of
    /// leaving it running against cleared state. Cancelling a child never propagates upward, so
    /// aborting one event's work cannot tear down the transport.
    pub cancel: CancellationToken,
}

impl InboundContext {
    /// Construct an inbound context.
    ///
    /// This is the supported way to create an [`InboundContext`] from outside the SDK,
    /// since the struct is `#[non_exhaustive]`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client_pubkey: impl Into<String>,
        request_event_id: impl Into<String>,
        is_encrypted: bool,
        mirrored_wrap_kind: Option<u16>,
        client_pmis: Option<Vec<String>>,
        payment_interaction: Option<PaymentInteractionMode>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            client_pubkey: client_pubkey.into(),
            request_event_id: request_event_id.into(),
            is_encrypted,
            mirrored_wrap_kind,
            client_pmis,
            payment_interaction,
            cancel,
        }
    }
}

/// A general-purpose inbound middleware, run as the final inbound stage before delivery to the MCP
/// handler.
///
/// `Send + Sync` (and `'static` via `Arc<dyn InboundMiddleware>`) so the detached chain future is
/// `Send`.
#[async_trait]
pub trait InboundMiddleware: Send + Sync {
    /// Inspect, transform, or gate one inbound message.
    ///
    /// To **forward**, return `next.run(message).await` (optionally with a transformed message).
    /// To **drop**, return `false` without calling `next`. Emitting an error or notification to the
    /// client is done through senders injected at construction, never via this return value.
    ///
    /// The returned bool is advisory (whether the downstream path reached the handler). Route
    /// cleanup is driven by the terminal-set flag inside [`Next`], not by this return, so a wrong
    /// return value can never pop a still-live route.
    async fn handle(&self, message: JsonRpcMessage, ctx: &InboundContext, next: Next) -> bool;
}

/// The continuation handed to a middleware. Owned/`Arc` so it is `Send + 'static` for the detached
/// chain. [`run`](Self::run) consumes it, so a middleware forwards at most once.
#[must_use = "call `run` to forward the message, `release` to drop a kept-alive request, or drop `Next` to gate (drop) it"]
pub struct Next {
    chain: Arc<[Arc<dyn InboundMiddleware>]>,
    index: usize,
    ctx: Arc<InboundContext>,
    tx: UnboundedSender<IncomingRequest>,
    // Carried so the terminal can record the forward on the request's writer slot, in the same
    // step as the send rather than after the whole chain returns.
    open_stream: ServerOpenStreamState,
    // Carried to rebuild `IncomingRequest` at the terminal (not on `InboundContext`).
    event: Option<Event>,
    // Set true only at the terminal; the single source of truth for drop-cleanup.
    reached: Arc<AtomicBool>,
    // Set when `keep_alive` is called. Used by `Drop`/`release` to reverse the route reservation.
    kept_alive: AtomicBool,
    // Set by `run` or `release` so `Drop` does not try to re-release an already-consumed Next.
    released: AtomicBool,
    // Stamped by a payment gate and attached to the terminal `IncomingRequest`.
    canonical_invocation_id: Mutex<Option<String>>,
    // Carried so `release` can pop the route and clean up wrap kinds when a kept-alive Next is
    // dropped after the chain runner has exited.
    event_routes: ServerEventRouteStore,
    request_wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>>,
}

impl Next {
    /// Keep the request's route and open-stream slot alive without forwarding the
    /// message yet. Used by middleware (e.g. payment gating) that parks a request
    /// and resolves it asynchronously from a foreign caller. The middleware still
    /// must call [`run`](Self::run) exactly once when the request is finally ready to
    /// forward, or call [`release`](Self::release) to drop it.
    pub fn keep_alive(&self) {
        self.kept_alive.store(true, Ordering::SeqCst);
        self.reached.store(true, Ordering::SeqCst);
        self.open_stream.mark_forwarded(&self.ctx.request_event_id);
    }

    /// Stamp this `Next` with a canonical invocation id. The terminal `IncomingRequest` will
    /// carry it. Used by the payment gate to surface the canonical identity to the handler.
    pub fn set_canonical_invocation_id(&self, id: String) {
        *self.canonical_invocation_id.lock().unwrap() = Some(id);
    }

    /// Advance the chain. At the end, mark reached, forward to the worker, and return `true`.
    pub async fn run(self, message: JsonRpcMessage) -> bool {
        self.released.store(true, Ordering::SeqCst);
        match self.chain.get(self.index) {
            None => {
                // Terminal: forward to the worker. `reached` is set ONLY here (the sole writer),
                // so route cleanup is authoritative regardless of what any middleware returns.
                // Record delivery only on a successful send: if the worker channel is closed
                // (shutdown), the route is released by cleanup instead of leaked until the sweep.
                let canonical_invocation_id = self.canonical_invocation_id.lock().unwrap().take();
                let delivered = self
                    .tx
                    .send(IncomingRequest {
                        message,
                        client_pubkey: self.ctx.client_pubkey.clone(),
                        event_id: self.ctx.request_event_id.clone(),
                        is_encrypted: self.ctx.is_encrypted,
                        canonical_invocation_id,
                        event: self.event.clone(),
                    })
                    .is_ok();
                self.reached.store(delivered, Ordering::SeqCst);
                if delivered {
                    // From here the request belongs to the handler and its response, so a later
                    // delivery's drop-cleanup must leave its open-stream writer alone.
                    self.open_stream.mark_forwarded(&self.ctx.request_event_id);
                }
                delivered
            }
            Some(mw) => {
                let mw = Arc::clone(mw);
                let next = Next {
                    chain: Arc::clone(&self.chain),
                    index: self.index + 1,
                    ctx: Arc::clone(&self.ctx),
                    tx: self.tx.clone(),
                    open_stream: self.open_stream.clone(),
                    event: self.event.clone(), // shared forward
                    reached: Arc::clone(&self.reached),
                    kept_alive: AtomicBool::new(false),
                    released: AtomicBool::new(false),
                    canonical_invocation_id: Mutex::new(None),
                    event_routes: self.event_routes.clone(),
                    request_wrap_kinds: Arc::clone(&self.request_wrap_kinds),
                };
                mw.handle(message, &self.ctx, next).await
            }
        }
    }

    /// Release a kept-alive request without forwarding it. Reverses `keep_alive`, pops the
    /// request's route and wrap-kind entry, and disposes any open-stream slot.
    pub async fn release(self) {
        self.released.store(true, Ordering::SeqCst);
        let event_id = self.ctx.request_event_id.clone();
        self.reached.store(false, Ordering::SeqCst);
        self.open_stream.unmark_forwarded(&event_id);

        let popped_route = self.event_routes.pop(&event_id).await;

        // Release the wrap-kind entry the transport reserved before dispatch (CEP-19),
        // but ONLY when this cleanup's own pop returned the route. See the same logic in
        // `run_inbound_chain`.
        if popped_route.is_some() {
            self.request_wrap_kinds.write().await.remove(&event_id);
        }

        // Reclaim the open-stream slot if this was the last chain run and it was un-forwarded.
        if let Some(slot) = self.open_stream.leave_chain(&event_id) {
            let token = slot.writer.progress_token().to_string();
            let client = slot.snapshot.client_pubkey.to_hex();
            self.open_stream
                .lock_token_index()
                .remove(&ServerOpenStreamState::client_token_key(&client, &token));
            slot.writer.dispose();
        }
    }
}

impl Drop for Next {
    fn drop(&mut self) {
        if self.kept_alive.load(Ordering::SeqCst) && !self.released.load(Ordering::SeqCst) {
            self.released.store(true, Ordering::SeqCst);

            // If the chain runner is still alive, it will observe `reached == false` and
            // `forwarded == false` and complete route + slot cleanup. If it has already exited,
            // spawn a cleanup task so a parked `Next` that is dropped after `keep_alive` does not
            // strand route state.
            let event_id = self.ctx.request_event_id.clone();
            let reached = Arc::clone(&self.reached);
            let open_stream = self.open_stream.clone();
            let event_routes = self.event_routes.clone();
            let request_wrap_kinds = Arc::clone(&self.request_wrap_kinds);

            tokio::spawn(async move {
                reached.store(false, Ordering::SeqCst);
                open_stream.unmark_forwarded(&event_id);

                let popped_route = event_routes.pop(&event_id).await;

                if popped_route.is_some() {
                    request_wrap_kinds.write().await.remove(&event_id);
                }

                if let Some(slot) = open_stream.leave_chain(&event_id) {
                    let token = slot.writer.progress_token().to_string();
                    let client = slot.snapshot.client_pubkey.to_hex();
                    open_stream
                        .lock_token_index()
                        .remove(&ServerOpenStreamState::client_token_key(&client, &token));
                    slot.writer.dispose();
                }
            });
        }
    }
}

/// Forward one inbound message through the middleware chain, or directly to the worker when the
/// chain is empty. Called from both seam sites (primary delivery and CEP-22 oversized re-inject).
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_inbound(
    middlewares: &Arc<[Arc<dyn InboundMiddleware>]>,
    tx: &UnboundedSender<IncomingRequest>,
    event_routes: &ServerEventRouteStore,
    open_stream: &ServerOpenStreamState,
    request_wrap_kinds: &Arc<RwLock<HashMap<String, Option<u16>>>>,
    cancel: &CancellationToken,
    message: JsonRpcMessage,
    client_pubkey: String,
    event_id: String,
    is_encrypted: bool,
    mirrored_wrap_kind: Option<u16>,
    client_pmis: Option<Vec<String>>,
    payment_interaction: Option<PaymentInteractionMode>,
    event: Option<Event>,
) {
    if middlewares.is_empty() {
        // Fast path: behavior-identical to the previous inline forward.
        let _ = tx.send(IncomingRequest {
            message,
            client_pubkey,
            event_id,
            is_encrypted,
            canonical_invocation_id: None,
            event,
        });
        return;
    }

    // Register this delivery's chain run against the request's writer slot BEFORE the chain is
    // spawned. This runs on the event-loop task, so a redelivery whose chain drops it without
    // waiting for this delivery cannot observe a zero count and reclaim the slot from under a
    // request that is about to forward.
    open_stream.enter_chain(&event_id);

    let ctx = Arc::new(InboundContext {
        client_pubkey,
        request_event_id: event_id,
        is_encrypted,
        mirrored_wrap_kind,
        client_pmis,
        payment_interaction,
        // A per-event child of the transport's shutdown token: closing the transport cancels
        // every in-flight event's work, while cancelling one event's token affects nothing else.
        cancel: cancel.child_token(),
    });
    spawn_inbound_chain(
        Arc::clone(middlewares),
        ctx,
        tx.clone(),
        event_routes.clone(),
        open_stream.clone(),
        Arc::clone(request_wrap_kinds),
        message,
        event,
    );
}

/// Detach the chain onto its own task so a slow middleware (e.g. a payment verification) never
/// blocks the single event-loop task that also drains the relay subscription.
#[allow(clippy::too_many_arguments)]
fn spawn_inbound_chain(
    chain: Arc<[Arc<dyn InboundMiddleware>]>,
    ctx: Arc<InboundContext>,
    tx: UnboundedSender<IncomingRequest>,
    event_routes: ServerEventRouteStore,
    open_stream: ServerOpenStreamState,
    request_wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>>,
    message: JsonRpcMessage,
    event: Option<Event>,
) {
    tokio::spawn(run_inbound_chain(
        chain,
        ctx,
        tx,
        event_routes,
        open_stream,
        request_wrap_kinds,
        message,
        event,
    ));
}

/// The awaitable chain runner. Awaited directly in tests; spawned in production.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_inbound_chain(
    chain: Arc<[Arc<dyn InboundMiddleware>]>,
    ctx: Arc<InboundContext>,
    tx: UnboundedSender<IncomingRequest>,
    event_routes: ServerEventRouteStore,
    open_stream: ServerOpenStreamState,
    request_wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>>,
    message: JsonRpcMessage,
    event: Option<Event>,
) {
    let event_id = ctx.request_event_id.clone();
    let reached = Arc::new(AtomicBool::new(false));
    let next = Next {
        chain,
        index: 0,
        ctx,
        tx,
        open_stream: open_stream.clone(),
        event,
        reached: Arc::clone(&reached),
        kept_alive: AtomicBool::new(false),
        released: AtomicBool::new(false),
        canonical_invocation_id: Mutex::new(None),
        event_routes: event_routes.clone(),
        request_wrap_kinds: Arc::clone(&request_wrap_kinds),
    };

    // Catch a middleware panic in-task: a panic in a detached tokio::spawn never reaches the event
    // loop, so we do not rely on JoinHandle. tokio::sync::RwLock does not poison and the session
    // write guard is already released, so AssertUnwindSafe is sound here.
    if let Err(panic) = AssertUnwindSafe(next.run(message)).catch_unwind().await {
        let cause = panic
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_string)
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_string());
        tracing::error!(
            target: LOG_TARGET,
            event_id = %event_id,
            cause = %cause,
            "inbound middleware chain panicked"
        );
    }

    // Drop-cleanup keyed on the terminal-set flag (not the middleware return), so a
    // forward-but-return-false middleware can never pop a delivered request's route (which would
    // make send_response fail "No client found"). A panic leaves `reached` false, so the route is
    // popped. On the primary request path a dropped notification reserved no route, so `pop` is a
    // no-op; the oversized re-inject path registers a route even for a reassembled notification, so
    // there `pop` is a real (harmless) removal.
    if !reached.load(Ordering::SeqCst) {
        let popped_route = event_routes.pop(&event_id).await;

        // Release the wrap-kind entry the transport reserved before dispatch (CEP-19),
        // but ONLY when this cleanup's own pop returned the route. A returned route
        // proves no concurrent responder consumed it, so no `send_response` for this
        // event can be between its route pop and its wrap-kind read; removing
        // unconditionally would race that pop-then-read and degrade wrap-kind mirroring
        // on the response. When the pop returns `None`, whoever consumed the route
        // (a responder, the stale-route sweep, session cleanup, or another duplicate's
        // cleanup) reclaims the wrap-kind entry itself, or no entry was ever inserted
        // (a dropped notification on the primary path).
        if popped_route.is_some() {
            request_wrap_kinds.write().await.remove(&event_id);
        }
    }

    // A gated (dropped) request also releases the open-stream slot the transport reserved for it
    // before dispatch: `send_response` never runs for a dropped request, so no arm of its
    // deferral decision can release the slot, the keepalive sweep cannot reap a never-started
    // writer, and the slots map is unbounded. The token index is keyed by
    // `(client_pubkey, progress_token)`, not by event id, so both keys are recovered from the slot
    // itself before it is dropped.
    //
    // Every chain run ends its registration here, delivered or not, and takes the slot with it
    // only when it was the last run for this request AND no delivery of the request ever reached
    // the handler. Relays redeliver, so two shapes must not reclaim it: a redelivery dropped while
    // the first delivery is still in the chain (the count is still non-zero), and one dropped
    // after that delivery forwarded (the slot is marked, and from the forward onwards it belongs
    // to `send_response`). Releasing in either case takes the writer away from a live request,
    // which the worker fetches only after the forward.
    if let Some(slot) = open_stream.leave_chain(&event_id) {
        let token = slot.writer.progress_token().to_string();
        let client = slot.snapshot.client_pubkey.to_hex();
        open_stream
            .lock_token_index()
            .remove(&ServerOpenStreamState::client_token_key(&client, &token));
        slot.writer.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{JsonRpcNotification, JsonRpcRequest};
    use crate::transport::open_stream::OpenStreamConfig;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    fn open_stream_state() -> ServerOpenStreamState {
        ServerOpenStreamState::new(&OpenStreamConfig::default(), 10)
    }

    fn req(id: &str, method: &str) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(id),
            method: method.to_string(),
            params: None,
        })
    }

    fn notif(method: &str) -> JsonRpcMessage {
        JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: None,
        })
    }

    fn chain_of(mws: Vec<Arc<dyn InboundMiddleware>>) -> Arc<[Arc<dyn InboundMiddleware>]> {
        Arc::from(mws)
    }

    /// Drive the chain to completion (awaited, not spawned, so cleanup is done on return) and
    /// return the delivered request, if any. Uses the shared `event_routes` for cleanup assertions.
    async fn drive(
        mws: Vec<Arc<dyn InboundMiddleware>>,
        message: JsonRpcMessage,
        event_id: &str,
        event_routes: &ServerEventRouteStore,
    ) -> Option<IncomingRequest> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(InboundContext {
            client_pubkey: "client_pk".to_string(),
            request_event_id: event_id.to_string(),
            is_encrypted: false,
            mirrored_wrap_kind: None,
            client_pmis: None,
            payment_interaction: None,
            cancel: CancellationToken::new(),
        });
        run_inbound_chain(
            chain_of(mws),
            ctx,
            tx,
            event_routes.clone(),
            open_stream_state(),
            Arc::new(RwLock::new(HashMap::new())),
            message,
            None,
        )
        .await;
        rx.try_recv().ok()
    }

    struct ForwardAll;
    #[async_trait]
    impl InboundMiddleware for ForwardAll {
        async fn handle(&self, message: JsonRpcMessage, _ctx: &InboundContext, next: Next) -> bool {
            next.run(message).await
        }
    }

    struct DropAll;
    #[async_trait]
    impl InboundMiddleware for DropAll {
        async fn handle(
            &self,
            _message: JsonRpcMessage,
            _ctx: &InboundContext,
            _next: Next,
        ) -> bool {
            false
        }
    }

    /// Forwards a transformed message (appends `::transformed` to a request method).
    struct AppendTransformed;
    #[async_trait]
    impl InboundMiddleware for AppendTransformed {
        async fn handle(
            &self,
            mut message: JsonRpcMessage,
            _ctx: &InboundContext,
            next: Next,
        ) -> bool {
            if let JsonRpcMessage::Request(ref mut r) = message {
                r.method = format!("{}::transformed", r.method);
            }
            next.run(message).await
        }
    }

    /// Records its id then forwards, to assert FIFO ordering.
    struct RecordOrder(Arc<Mutex<Vec<u8>>>, u8);
    #[async_trait]
    impl InboundMiddleware for RecordOrder {
        async fn handle(&self, message: JsonRpcMessage, _ctx: &InboundContext, next: Next) -> bool {
            self.0.lock().unwrap().push(self.1);
            next.run(message).await
        }
    }

    /// Forwards (reaching the terminal), then panics in its own post-processing.
    struct ForwardThenPanic;
    #[async_trait]
    impl InboundMiddleware for ForwardThenPanic {
        async fn handle(&self, message: JsonRpcMessage, _ctx: &InboundContext, next: Next) -> bool {
            let _ = next.run(message).await;
            panic!("post-forward boom");
        }
    }

    struct Panicker;
    #[async_trait]
    impl InboundMiddleware for Panicker {
        async fn handle(
            &self,
            _message: JsonRpcMessage,
            _ctx: &InboundContext,
            _next: Next,
        ) -> bool {
            panic!("middleware boom");
        }
    }

    /// Forwards (reaches the terminal) but wrongly returns `false`.
    struct ForwardButReturnFalse;
    #[async_trait]
    impl InboundMiddleware for ForwardButReturnFalse {
        async fn handle(&self, message: JsonRpcMessage, _ctx: &InboundContext, next: Next) -> bool {
            let _ = next.run(message).await;
            false
        }
    }

    #[test]
    fn empty_chain_forwards_inline_unchanged() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let routes = ServerEventRouteStore::new();
        let empty: Arc<[Arc<dyn InboundMiddleware>]> = Arc::from(Vec::new());

        dispatch_inbound(
            &empty,
            &tx,
            &routes,
            &open_stream_state(),
            &Arc::new(RwLock::new(HashMap::new())),
            &CancellationToken::new(),
            req("1", "tools/call"),
            "client_pk".to_string(),
            "e1".to_string(),
            false,
            None, // mirrored_wrap_kind
            None, // client_pmis
            None, // payment_interaction
            None, // event
        );

        let got = rx.try_recv().expect("empty chain forwards inline");
        assert_eq!(got.event_id, "e1");
        assert_eq!(got.client_pubkey, "client_pk");
    }

    #[tokio::test]
    async fn forward_delivers() {
        let routes = ServerEventRouteStore::new();
        let got = drive(
            vec![Arc::new(ForwardAll)],
            req("1", "tools/call"),
            "e1",
            &routes,
        )
        .await;
        assert!(got.is_some());
    }

    #[tokio::test]
    async fn drop_does_not_deliver_and_pops_route() {
        let routes = ServerEventRouteStore::new();
        routes
            .register(
                "e1".to_string(),
                "client_pk".to_string(),
                serde_json::json!("1"),
                None,
            )
            .await;
        let got = drive(
            vec![Arc::new(DropAll)],
            req("1", "tools/call"),
            "e1",
            &routes,
        )
        .await;
        assert!(got.is_none());
        assert!(
            !routes.has_event_route("e1").await,
            "dropped request must release its route"
        );
    }

    #[tokio::test]
    async fn transform_changes_downstream_message() {
        let routes = ServerEventRouteStore::new();
        let got = drive(
            vec![Arc::new(AppendTransformed)],
            req("1", "tools/call"),
            "e1",
            &routes,
        )
        .await
        .expect("forwarded");
        match got.message {
            JsonRpcMessage::Request(r) => assert_eq!(r.method, "tools/call::transformed"),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runs_in_registration_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let routes = ServerEventRouteStore::new();
        let got = drive(
            vec![
                Arc::new(RecordOrder(log.clone(), 1)),
                Arc::new(RecordOrder(log.clone(), 2)),
            ],
            req("1", "tools/call"),
            "e1",
            &routes,
        )
        .await;
        assert!(got.is_some());
        assert_eq!(*log.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn panic_is_caught_and_route_popped() {
        let routes = ServerEventRouteStore::new();
        routes
            .register(
                "e1".to_string(),
                "client_pk".to_string(),
                serde_json::json!("1"),
                None,
            )
            .await;
        // The panic is caught inside run_inbound_chain; this await must return normally.
        let got = drive(
            vec![Arc::new(Panicker)],
            req("1", "tools/call"),
            "e1",
            &routes,
        )
        .await;
        assert!(got.is_none());
        assert!(
            !routes.has_event_route("e1").await,
            "panicked chain must release its route"
        );
    }

    #[tokio::test]
    async fn forward_then_panic_keeps_the_delivered_route() {
        // A panic AFTER the terminal delivered must NOT pop the live route (reached stays true).
        let routes = ServerEventRouteStore::new();
        routes
            .register(
                "e1".to_string(),
                "client_pk".to_string(),
                serde_json::json!("1"),
                None,
            )
            .await;
        let got = drive(
            vec![Arc::new(ForwardThenPanic)],
            req("1", "tools/call"),
            "e1",
            &routes,
        )
        .await;
        assert!(got.is_some(), "message was delivered before the panic");
        assert!(
            routes.has_event_route("e1").await,
            "a panic after delivery must not pop the live route"
        );
    }

    #[tokio::test]
    async fn forward_but_return_false_still_delivers_and_keeps_route() {
        // Cleanup keys on the terminal-set flag, not the (wrong) middleware return.
        let routes = ServerEventRouteStore::new();
        routes
            .register(
                "e1".to_string(),
                "client_pk".to_string(),
                serde_json::json!("1"),
                None,
            )
            .await;
        let got = drive(
            vec![Arc::new(ForwardButReturnFalse)],
            req("1", "tools/call"),
            "e1",
            &routes,
        )
        .await;
        assert!(got.is_some(), "message must still reach the worker");
        assert!(
            routes.has_event_route("e1").await,
            "a delivered request's route must survive a wrong return value"
        );
    }

    /// Run the chain with an explicit wrap-kind map (the [`drive`] helper hides it).
    async fn drive_with_wrap_kinds(
        mws: Vec<Arc<dyn InboundMiddleware>>,
        message: JsonRpcMessage,
        event_id: &str,
        event_routes: &ServerEventRouteStore,
        wrap_kinds: &Arc<RwLock<HashMap<String, Option<u16>>>>,
    ) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let ctx = Arc::new(InboundContext {
            client_pubkey: "client_pk".to_string(),
            request_event_id: event_id.to_string(),
            is_encrypted: false,
            mirrored_wrap_kind: None,
            client_pmis: None,
            payment_interaction: None,
            cancel: CancellationToken::new(),
        });
        run_inbound_chain(
            chain_of(mws),
            ctx,
            tx,
            event_routes.clone(),
            open_stream_state(),
            Arc::clone(wrap_kinds),
            message,
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn gated_drop_releases_request_wrap_kinds() {
        let routes = ServerEventRouteStore::new();
        let wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // A dropped request whose route is still present at cleanup: the entry the
        // transport reserved before dispatch must be released.
        routes
            .register(
                "e1".to_string(),
                "client_pk".to_string(),
                serde_json::json!("1"),
                None,
            )
            .await;
        wrap_kinds
            .write()
            .await
            .insert("e1".to_string(), Some(1059));
        drive_with_wrap_kinds(
            vec![Arc::new(DropAll)],
            req("1", "tools/call"),
            "e1",
            &routes,
            &wrap_kinds,
        )
        .await;
        assert!(
            !wrap_kinds.read().await.contains_key("e1"),
            "a dropped request must release its wrap-kind entry"
        );

        // Positive control: the same probe sees a delivered request's entry SURVIVE
        // (cleanup never runs when the terminal was reached), proving the assertion
        // above can observe insertions at all.
        routes
            .register(
                "e2".to_string(),
                "client_pk".to_string(),
                serde_json::json!("2"),
                None,
            )
            .await;
        wrap_kinds
            .write()
            .await
            .insert("e2".to_string(), Some(1059));
        drive_with_wrap_kinds(
            vec![Arc::new(ForwardAll)],
            req("2", "tools/call"),
            "e2",
            &routes,
            &wrap_kinds,
        )
        .await;
        assert!(
            wrap_kinds.read().await.contains_key("e2"),
            "a delivered request's entry belongs to send_response, not the cleanup"
        );
    }

    #[tokio::test]
    async fn drop_cleanup_with_a_consumed_route_leaves_the_wrap_kind_entry() {
        // The guard: when the cleanup's own route pop returns None (a concurrent
        // responder consumed the route first), the wrap-kind entry must SURVIVE, since
        // that responder may be between its route pop and its wrap-kind read, and its
        // own cleanup reclaims the entry after publish. No route is registered here,
        // which deterministically models the consumed-route case.
        let routes = ServerEventRouteStore::new();
        let wrap_kinds: Arc<RwLock<HashMap<String, Option<u16>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        wrap_kinds
            .write()
            .await
            .insert("e3".to_string(), Some(1059));

        drive_with_wrap_kinds(
            vec![Arc::new(DropAll)],
            req("3", "tools/call"),
            "e3",
            &routes,
            &wrap_kinds,
        )
        .await;
        assert!(
            wrap_kinds.read().await.contains_key("e3"),
            "a consumed route means another party owns (and reclaims) the entry"
        );
    }

    #[tokio::test]
    async fn notification_forwards_and_drop_is_noop() {
        let routes = ServerEventRouteStore::new();
        let got = drive(
            vec![Arc::new(ForwardAll)],
            notif("notifications/progress"),
            "e1",
            &routes,
        )
        .await;
        assert!(got.is_some(), "a forwarded notification reaches the worker");

        // A dropped notification reserved no route on the primary path; cleanup is a harmless no-op.
        let got2 = drive(
            vec![Arc::new(DropAll)],
            notif("notifications/progress"),
            "e2",
            &routes,
        )
        .await;
        assert!(got2.is_none());
        assert!(!routes.has_event_route("e2").await);
    }
}
