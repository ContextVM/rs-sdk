# Payments Guide (CEP-8)

CEP-8 adds capability pricing and payments to ContextVM: a server can price
individual capabilities, and a client can pay for them either transparently
(side-band notifications while the request stays in flight) or through
explicit gating (the request is refused with a payment error until paid).
This guide covers both sides of the wire as this SDK ships them.

CEP-8 support ships in two phases. What ships today is Phase A: the full
protocol machinery on both sides, with deterministic fakes
(`FakePaymentProcessor` and `FakePaymentHandler`, behind the `test-utils`
feature) standing in for real payment rails. Phase B is the rails themselves
(Lightning over NWC/NIP-47, LNURL, LNbits), which plug into the same
`PaymentProcessor` / `PaymentHandler` traits without API changes.

The protocol reference is the CEP-8 specification in the ContextVM
documentation repository.

## The two lifecycles

Transparent (the default): the request stays pending while payment settles
out of band of the JSON-RPC conversation.

```text
client                                server
  |  tools/call (priced)                |
  |------------------------------------>|
  |  notifications/payment_required     |
  |<------------------------------------|
  |     (client pays; server verifies)  |
  |  notifications/payment_accepted     |
  |<------------------------------------|
  |  result                             |
  |<------------------------------------|
```

Explicit gating (opt-in per session via `payment_interaction`): the request
is answered immediately with an error carrying payment options, and a paid
retry of the same invocation executes.

```text
client                                server
  |  tools/call (priced)                |
  |------------------------------------>|
  |  error -32042 Payment Required      |
  |<------------------------------------|
  |     (client pays one option)        |
  |  tools/call (same method + params)  |
  |------------------------------------>|
  |  error -32043 Payment Pending       |   (only while verification runs)
  |<------------------------------------|
  |  tools/call (retry after backoff)   |
  |------------------------------------>|
  |  result                             |
  |<------------------------------------|
```

## Server setup

`with_server_payments` is the production registration entry point. It wires
the announcement tags, the negotiation policy, and both lifecycle middlewares
so they agree with each other and with the wire:

```rust
use contextvm_sdk::payments::{with_server_payments, ServerPaymentsOptions};
use contextvm_sdk::payments::types::PricedCapability;

let options = ServerPaymentsOptions::new(
    vec![my_processor],                 // one PaymentProcessor per PMI
    vec![PricedCapability {
        method: "tools/call".to_string(),
        name: Some("expensive-tool".to_string()),
        amount: 21,
        max_amount: None,
        currency_unit: "sats".to_string(),
        description: None,
    }],
);
with_server_payments(&mut server_transport, options)?;
server_transport.start().await?;
```

Server options:

| Field | Default | Meaning |
|---|---|---|
| `processors` | required | One `PaymentProcessor` per PMI; the first is the fallback when the client advertises no matching PMI |
| `priced_capabilities` | required | Capability patterns that are priced; first match wins |
| `resolve_price` | `None` | Dynamic pricing callback: quote, reject, or waive per invocation |
| `payment_ttl` | 300 s | How long a request stays in pending-payment state |
| `max_pending_payments` | 1000 | Cap on concurrently tracked pending payments |
| `payment_interaction` | `Optional` | Accept both lifecycles (`Optional`) or transparent only (`Transparent`) |

Registration contract: call it exactly once, after constructing the transport
and before `start()`, and before `announce()` (or the first announcement
ships without payment tags). Both misuses are refused with an error before
any state changes. The function owns the announcement extra-tag and
pricing-tag slots; do not call the tag setters around it.

Rate limiting is the deployment's job: every successful payment offer, in
either lifecycle, spawns one detached verification task, unbounded across
identities and unauthenticated peers. `max_pending_payments` and the
authorization store's entry caps bound state, not tasks. CEP-8 explicitly
leaves rate limiting and abuse prevention to implementations, so deployments
that cannot trust their peer set should bound intake upstream:
`allowed_public_keys`, relay-side policy, or an external limiter.

All payment state is in-memory and single-process on both sides: pending
payments, grants, dedup windows, and payment route snapshots are forgotten on
restart, and an evicted authorization re-invoices its payer on retry.

Keep `payment_ttl` at or below the transport's session timeout: a payment
that outlives the session costs the client the acceptance notification even
though the paid result still delivers from the captured route snapshot (the
registration logs a warning for that configuration).

## Client setup

`with_client_payments` is the client peer. It advertises the handlers' PMIs,
optionally requests a payment interaction mode, and installs the payment
engine into the transport's inbound path:

```rust
use contextvm_sdk::payments::{with_client_payments, ClientPaymentsOptions};

let options = ClientPaymentsOptions::new()
    .with_handlers(vec![my_wallet_handler])   // in-band auto-pay
    .with_payment_policy(my_spending_policy); // the amount / replay gate
with_client_payments(&mut client_transport, options)?;
client_transport.start().await?;
```

Client options:

| Field | Default | Meaning |
|---|---|---|
| `handlers` | empty | In-band wallet handlers, one per PMI; empty is the out-of-band shape |
| `synthetic_progress_interval` | 30 s | Interval between keep-alive heartbeats (one extra beat fires immediately) |
| `default_payment_ttl` | 300 s | Keep-alive window when `payment_required` carries no `ttl` |
| `payment_policy` | `None` (approve) | Async gate evaluated before the handler's own `can_handle` |
| `payment_interaction` | `None` | `Some(mode)` requests that mode; `None` leaves the transport config's seed alone |
| `max_pending_retries` | 10 | Cap on `-32043` retries (checked before each increment, so up to 11 total sends) |
| `on_payment_required` | `None` | Explicit-gating `-32042` handler; `None` forwards the raw error |

Registration contract: exactly once, after construction, before `start()`.
A started, closed, or already-registered transport is refused with an error
before any state changes.

PMI advertisement REPLACES the configured list: the handlers' PMIs are
advertised in registration order and replace anything seeded through
`NostrClientTransportConfig::pmis`, mirroring the reference implementation.
The supported shape is handlers-own-the-PMIs; a client with no handlers
advertises no PMIs even when the config seeded some.

Three client shapes fall out of the options:

- The wallet client: handlers configured, transparent session. Offers are
  paid in-band, gated by `payment_policy` and the handler's `can_handle`.
- The out-of-band client: no handlers. The `payment_required` notification is
  forwarded to the application, synthetic progress keeps the request alive
  while the human or an external system pays, and the server's verification
  completes the flow.
- The agent host: `payment_interaction: ExplicitGating` plus
  `on_payment_required`, usually with no handlers. Every payment decision
  surfaces as an explicit callback; the engine retries the original request
  after the callback reports payment.

The proxy carries a twin of this configuration:
`ProxyConfig::with_payment_options(options)` registers payments on the
proxy's transport at construction, on both the `NostrMCPProxy::new` path and
the `serve_client_handler` path. Hand-wired transports (for example a
`NostrClientWorker` built directly) call `with_client_payments` themselves
before serving.

## The transparent lifecycle, end to end

On a priced invocation the server emits `notifications/payment_required`
correlated to the request event, waits for its processor to verify
settlement, emits `notifications/payment_accepted`, and only then forwards
the request to the MCP handler. Duplicate deliveries of one request event
share one payment within the configured bounds.

On the client, a correlated `payment_required` runs this pipeline: parse
(a malformed offer engages nothing and the raw notification still reaches
the application), the mode-mismatch guard (below), keep-alive registration,
handler lookup by PMI, an in-flight dedup keyed by the offer's `pay_req`,
then `payment_policy`, `can_handle`, and `handle` on a detached task. When
the request carries a progress token, the consumer sees the immediate
keep-alive beat first, then the forwarded notification.

What the application observes per outcome:

- Paid and delivered: the notification, keep-alive progress beats, the
  acceptance, then the real result.
- Declined by policy or by the handler: the notification, then a synthesized
  `-32000` error on the original request id (`"Payment declined by client
  policy"` or `"Payment declined by client handler"`, with the PMI, amount,
  and, when available, the original method and capability in `data`).
- Handler failed: the notification only. Nothing is synthesized, the request
  stays pending, keep-alives run to the TTL, and the request resolves by the
  server's TTL or the caller's own timeout.
- Rejected by the server (`payment_rejected`): a synthesized `-32000`
  `"Payment rejected[: message]"` on the original id; the rejection
  notification itself is not forwarded.
- No handler for the PMI: the notification (pay out of band); the result
  arrives when the server verifies.
- Verification never completes: keep-alives stop at the TTL and the caller's
  own request timeout fires.

Mode mismatch: a client that requested `explicit_gating` in a session where
the server did not accept it (including a server that disclosed nothing)
never auto-satisfies transparent offers; it synthesizes a decline instead,
per the CEP-8 negotiation rules. The inverse cell is deliberate and mirrors
the reference implementation: when the server DID accept explicit gating,
a transparent offer arriving in that session is still auto-paid,
policy-gated.

## The explicit-gating lifecycle, end to end

The server answers a priced invocation with `-32042 Payment Required`
carrying `payment_options`, verifies the payment on a detached task, and
records a single-use grant keyed by the canonical invocation identity (the
client pubkey plus the JCS hash of `method` and `params`, with `_meta` and
ids excluded). A retry during verification draws `-32043 Payment Pending`
with a `retry_after`; a retry after verification claims the grant and
executes.

The client engine intercepts both codes before they reach the consumer.
For `-32042` it calls `on_payment_required` with the options, instructions,
and the original request; on `paid: true` it re-sends the cached original
request byte for byte (same JSON-RPC id, same `method` and `params`
including `_meta`, a fresh outer event), which CEP-8 sanctions for retry
matching. For `-32043` it re-sends after a capped exponential backoff seeded
by the server's `retry_after` (factor 1.5, capped at 10 s, floored at 1 s).

The floor is a deliberate protective divergence from the reference client: a
`retry_after: 0` server would otherwise trigger a zero-delay retry whose
byte-identical event can mint the same second-resolution event id as the
original, which relays and the server's ingestion dedup swallow.

Exactly these outcomes reach the consumer for the two payment codes;
everything else is handled internally:

1. The raw `-32042`, when no `on_payment_required` is configured.
2. The raw `-32042`, when the original request is no longer cached.
3. A synthesized `-32042` with `data.reason` (default `"user_cancelled"`),
   when the callback reports `paid: false`.
4. A synthesized `-32042` with `data.reason` and
   `data.type: "payment_handler_error"`, when the callback fails.
5. The raw `-32043`, when the original request is no longer cached.
6. The raw `-32043`, when `max_pending_retries` is exhausted.
7. A synthesized `-32043` `"Failed to retry pending request"`, when the
   backoff re-send itself fails.

One internal failure is deliberately silent: if the re-send after a
`paid: true` outcome itself fails, nothing is synthesized (the list above
stays exhaustive); the failure is logged and the caller's own request timeout
resolves the attempt, matching the reference implementation's
consumer-observable outcome.

Degenerate shapes (a `-32042` without a non-empty `payment_options` array, a
`-32043` without `retry_after`) are never classified and pass through
untouched. Every surfaced error above carries the ORIGINAL inner request id,
resolved through the transport's correlation entry, so the client behaves
identically against servers that answer with the inner id (this SDK) and
servers that answer with the rewritten event id (the reference SDK).

There is no cap on `-32042` cycles: a verification failure clears the
server's pending state, so a paid retry can draw a fresh offer with a new
invoice, and the callback is consulted again each round. The callback is the
gate; an agent host that wants a budget enforces it there.

## Operational notes for auto-pay operators

Threat model. Every inbound byte the engine reacts to is an untrusted
server's. Two transport gates run before the engine: events not signed by
the configured server are dropped, and correlated messages that match no
live pending request are dropped, so a third party cannot address the
client at all and a replayed or forged invoice for a request that is not in
flight never reaches a handler. What no gate can judge is a fresh offer from
the real server for a request that is still pending: amounts, TTLs, and
repeat offers are exactly what `payment_policy` exists to gate. Treat that
callback as the spending limit. The in-flight dedup is keyed by the offer's
`pay_req` and released when the handler settles, matching the reference
implementation, so a distinct re-offer for the same still-pending request is
honored (policy-gated). And as noted above, a session where the server
accepted explicit gating still auto-pays transparent offers, policy-gated.

Synthetic progress is fabricated. The keep-alive heartbeat emits
`notifications/progress` with `progress: 0` toward the local consumer while
a payment settles, carrying the request's original progress token with its
exact JSON type. A consumer cannot distinguish a fabricated beat from a real
server-sent one; the reference implementation behaves identically. The
fabrication window is bounded by the offer's `ttl` (or
`default_payment_ttl`). A request without a progress token gets no
heartbeat; callers that rely on progress-aware timeouts should request
progress.

Long payments and the correlation sweep. The client transport retains
response correlation for `config.timeout` (default 30 s) and sweeps stale
entries. For every payment, the engine runs a keep-alive touch loop on a
cadence of `synthetic_progress_interval` or half the retention timeout,
whichever is smaller, so the correlation entry survives for the payment's
lifetime with real margin regardless of heartbeat settings or whether the
request carried a progress token. The loop stops with the payment (terminal
response, acceptance, rejection, decline, TTL expiry, or transport close).

Two timeout outcomes are documented rather than surfaced as errors, matching
the transport's behavior for all traffic in those states: a gating error
whose correlation entry was already swept is dropped at the correlation
gate, and one arriving without a correlation tag is dropped as uncorrelated;
in both cases the consumer sees its own request timeout.

Rate limiting is upstream's job on the server side. As the server-setup
section above states, every successful offer spawns one unbounded detached
verification task; auto-pay deployments talking to untrusted servers face the
mirror concern (a hostile server can stream fresh offers), and
`payment_policy` plus the in-flight dedup are the client-side bounds.

Payment requests are never logged. No log line on either side includes a
`pay_req` (an invoice can encode amount, recipient, and description); logs
carry PMIs, amounts, counts, and event ids instead.

No result replay is owed. Per CEP-8, a paid authorization grants execution,
not redelivery: if the result is lost to transport failure after execution,
a later matching invocation with no unused authorization may be treated as
unpaid and draw a fresh offer.

## Configuration reference

Shipped defaults, shared by both sides where they mirror each other:

| Constant | Value |
|---|---|
| Default payment TTL | 300 s |
| Default synthetic progress interval | 30 s |
| Default max pending payments (server) | 1000 |
| Default max pending retries (client) | 10 |
| Raw-request cache capacity (client) | 1000 entries |
| Phase A PMI | `bitcoin-lightning-bolt11` (the fakes use `"fake"`) |

The `-32042` / `-32043` / `-32602` codes, the three notification methods,
and the wire field orders are pinned by the conformance suite against the
reference implementation.

## Phase B: payment rails

The `PaymentProcessor` (server: create and verify a payment request) and
`PaymentHandler` (client: settle one) traits are the extension surface.
Planned rails include Lightning BOLT11 via NWC (NIP-47), LNURL (NIP-57
zaps), and LNbits REST; all are deliberately deferred, and the options
structs are non-exhaustive so rails can add configuration without breaking
changes.
