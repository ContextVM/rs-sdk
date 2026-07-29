//! Inbound middleware seam for the server transport (rs-sdk port of TS `addInboundMiddleware`).
//!
//! An ordered chain of [`InboundMiddleware`] runs as the final inbound stage, just before a message
//! is forwarded to the rmcp worker. A middleware **forwards** by calling [`Next::run`] and **drops**
//! by returning without calling it. The chain is behavior-preserving until a middleware is
//! registered: with an empty chain, `dispatch_inbound` reproduces today's direct
//! `tx.send(IncomingRequest)`.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::FutureExt; // .catch_unwind()
use nostr_sdk::prelude::Event;
use tokio::sync::mpsc::UnboundedSender;

use super::{IncomingRequest, ServerEventRouteStore, LOG_TARGET};
use crate::core::types::{JsonRpcMessage, PaymentInteractionMode};

/// Per-event context handed to every inbound middleware.
///
/// Built once per event and shared (by `Arc`) across the whole chain. The four transport-level
/// fields are populated at the seam; `client_pmis` and `payment_interaction` stay `None` until
/// CEP-8 negotiation fills them.
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
    /// Client-advertised payment method ids. `None` until CEP-8 negotiation populates it.
    pub client_pmis: Option<Vec<String>>,
    /// Effective negotiated payment interaction mode. `None` until CEP-8 negotiation populates it.
    pub payment_interaction: Option<PaymentInteractionMode>,
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
#[must_use = "call `run` to forward the message, or drop `Next` to gate (drop) it"]
pub struct Next {
    chain: Arc<[Arc<dyn InboundMiddleware>]>,
    index: usize,
    ctx: Arc<InboundContext>,
    tx: UnboundedSender<IncomingRequest>,
    // Carried to rebuild `IncomingRequest` at the terminal (not on `InboundContext`).
    event: Option<Event>,
    // Set true only at the terminal; the single source of truth for drop-cleanup.
    reached: Arc<AtomicBool>,
}

impl Next {
    /// Advance the chain. At the end, mark reached, forward to the worker, and return `true`.
    pub async fn run(self, message: JsonRpcMessage) -> bool {
        match self.chain.get(self.index) {
            None => {
                // Terminal: forward to the worker. `reached` is set ONLY here (the sole writer),
                // so route cleanup is authoritative regardless of what any middleware returns.
                // Record delivery only on a successful send: if the worker channel is closed
                // (shutdown), the route is released by cleanup instead of leaked until the sweep.
                let delivered = self
                    .tx
                    .send(IncomingRequest {
                        message,
                        client_pubkey: self.ctx.client_pubkey.clone(),
                        event_id: self.ctx.request_event_id.clone(),
                        is_encrypted: self.ctx.is_encrypted,
                        event: self.event,
                    })
                    .is_ok();
                self.reached.store(delivered, Ordering::SeqCst);
                delivered
            }
            Some(mw) => {
                let mw = Arc::clone(mw);
                let next = Next {
                    chain: Arc::clone(&self.chain),
                    index: self.index + 1,
                    ctx: Arc::clone(&self.ctx),
                    tx: self.tx.clone(),
                    event: self.event, // moved forward
                    reached: Arc::clone(&self.reached),
                };
                mw.handle(message, &self.ctx, next).await
            }
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
    message: JsonRpcMessage,
    client_pubkey: String,
    event_id: String,
    is_encrypted: bool,
    mirrored_wrap_kind: Option<u16>,
    event: Option<Event>,
) {
    if middlewares.is_empty() {
        // Fast path: behavior-identical to the previous inline forward.
        let _ = tx.send(IncomingRequest {
            message,
            client_pubkey,
            event_id,
            is_encrypted,
            event,
        });
        return;
    }

    let ctx = Arc::new(InboundContext {
        client_pubkey,
        request_event_id: event_id,
        is_encrypted,
        mirrored_wrap_kind,
        client_pmis: None,
        payment_interaction: None,
    });
    spawn_inbound_chain(
        Arc::clone(middlewares),
        ctx,
        tx.clone(),
        event_routes.clone(),
        message,
        event,
    );
}

/// Detach the chain onto its own task so a slow middleware (e.g. a payment verification) never
/// blocks the single event-loop task that also drains the relay subscription.
fn spawn_inbound_chain(
    chain: Arc<[Arc<dyn InboundMiddleware>]>,
    ctx: Arc<InboundContext>,
    tx: UnboundedSender<IncomingRequest>,
    event_routes: ServerEventRouteStore,
    message: JsonRpcMessage,
    event: Option<Event>,
) {
    tokio::spawn(run_inbound_chain(
        chain,
        ctx,
        tx,
        event_routes,
        message,
        event,
    ));
}

/// The awaitable chain runner. Awaited directly in tests; spawned in production.
async fn run_inbound_chain(
    chain: Arc<[Arc<dyn InboundMiddleware>]>,
    ctx: Arc<InboundContext>,
    tx: UnboundedSender<IncomingRequest>,
    event_routes: ServerEventRouteStore,
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
        event,
        reached: Arc::clone(&reached),
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
        event_routes.pop(&event_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{JsonRpcNotification, JsonRpcRequest};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

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
        });
        run_inbound_chain(chain_of(mws), ctx, tx, event_routes.clone(), message, None).await;
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
            req("1", "tools/call"),
            "client_pk".to_string(),
            "e1".to_string(),
            false,
            None,
            None,
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
