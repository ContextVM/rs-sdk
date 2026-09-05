# Plan — CEP-8 payments through `contextvm-ffi`

Goal: expose the rs-sdk CEP-8 payment stack (negotiation, gating, wire types,
authorization store, targeted responses) through the FFI so a foreign consumer
(cvm-worker, Kotlin/UniFFI) can run **paid tools over ContextVM** with the same
economics famulus had over DVM: quote → BOLT11 → settle → execute.

Status of the SDK side (as of HEAD): primitives, negotiation, authorization
store, canonical invocation identity, and `send_targeted_response` have landed,
but **the payment middleware itself has not** (CHANGELOG: *"the middleware that
consumes these arrives later"*). This plan therefore puts the gate middleware in
the FFI crate, built entirely from public SDK APIs, so it does not block on SDK
progress. When the SDK middleware lands, the FFI can switch its internals to it
without changing the foreign API.

## Non-goals (explicitly out of scope)

- **Client-side payments** (`PaymentHandler`, `with_pmis`,
  `get_effective_payment_interaction`) — that is the web client's job, not the
  worker's. The SDK client transport already has it natively.
- **C-ABI (`cvm_server_ch_*`) payment surface.** The only consumer today is
  Kotlin via UniFFI. Adding C functions without a C consumer is scaffolding.
  Revisit when one exists (the gate internals are shared either way).
- **`ResolvePrice` in Rust.** Dynamic pricing (audio vs full, platform premium)
  stays in Kotlin — the foreign side computes the quote when it creates the
  invoice and submits the amount back. The Rust gate is price-agnostic. This
  removes the only reason the FFI would need a pricing *callback*.
- **Result idempotency storage.** Terminal-result records stay in the
  consumer's store (cvm-worker's `PaymentSessionStore`), but the *replay
  decision* is gate-integrated (§D3 “Result replay”): the gate emits the
  event, the consumer answers with `mark_replayed` or `submit_invoice`.
  Crash recovery of payment state is also in scope (§Restart model).

## Current blockers (why the FFI can't do payments today)

1. `Server::new` (UniFFI) calls `transport.start()` **inside the constructor**.
   Both `set_supported_payment_interaction` and `add_inbound_middleware` must
   run *before* `start()` — there is no window to configure either.
2. Nothing payment-related is exposed: no policy setter, no middleware
   registration, no targeted response, no payment events for a foreign wallet.
3. `targeted_response_sender()` must be built **after** pricing/extra tags are
   set (it captures them). The current construct-then-configure order makes the
   correct order impossible to express.

## Design decisions

### D1 — Channel-driven foreign seam, no foreign callbacks

The wallet (Spark Breez / NWC) lives in Kotlin and is async. Two ways to reach
it from the Rust gate:

- **(A) UniFFI callback interface** — Kotlin implements `PaymentProcessor`,
  Rust awaits foreign futures. Requires async foreign callbacks on uniffi 0.31
  (unproven in this repo), foreign-object lifetime management across detached
  tokio tasks, and JNI-on-tokio-thread error paths.
- **(B) Event channel** — the gate *parks* the gated request and pushes a
  `PaymentGateRequest` onto an outbound queue; Kotlin drains it, creates the
  invoice with its own wallet, and calls `submit_invoice` / `mark_settled` /
  `mark_failed`. **Chosen.**

(B) matches the FFI's existing idiom (everything is already a recv loop),
requires zero new foreign-callback machinery, is testable from pure Rust, and
maps 1:1 onto what cvm-worker already does today (its wallet poll loop barely
changes — it just moves from inside the tool to the server event loop).

### D2 — The gate middleware lives in the FFI crate

New module `contextvm-ffi/src/payment_gate.rs` implementing the SDK's public
`InboundMiddleware` trait, consuming:

| SDK piece (public, already landed) | Use |
|---|---|
| `InboundMiddleware` / `Next` (`transport::server::middleware`) | intercept `tools/call`, park or forward |
| `InboundContext.payment_interaction` | which lifecycle the session negotiated |
| `compute_canonical_invocation_identity` | dedupe/authorize retries (`_meta`-stripped JCS hash) |
| `AuthorizationStore` | pending / granted (single-use `claim`) |
| wire types + constants (`PaymentRequiredParams`, `-32042`/`-32043`, PMI) | exact CEP-8 wire bytes |
| `targeted_response_sender()` | answer a request without consuming its route |
| `send_notification` (via captured `Arc<Mutex<NostrServerTransport>>`) | transparent-lifecycle notifications |

### D3 — Two lifecycles, one state machine

Mode comes from the negotiated session (`InboundContext::payment_interaction`),
fallback transparent.

**State model (both lifecycles).** A gated canonical identity moves through:
`None → AwaitingInvoice → InvoiceIssued(pay_req) → Granted | Cleared`, with a
third exit from `AwaitingInvoice`: `Replayed` (already-paid result cache hit,
forwarded free — see “Result replay” below). The sub-states matter because
the pre-invoice window has no `pay_req` to re-emit.

**Result replay is NOT orthogonal to the gate — it is the third exit.** The
gate runs before any Kotlin dispatch, so a retry of a completed call is
gated again before `PaymentSessionStore` could answer from cache, and the
forwarded `IncomingRequest` carries no canonical id (its result key today
includes the JSON-RPC id, which never matches a retry). Resolution, without
a second round trip: the replay decision rides the **existing** gate event.
In `AwaitingInvoice` the consumer resolves the canonical id against its
result store and answers the gate with one of:

- `submit_invoice(...)` — no terminal result: quote, invoice, proceed.
- `mark_replayed(event_id)` — terminal result (or replay-eligible) exists:
  the gate forwards the parked message to the handler immediately, free, no
  `-32042` ever sent (gating) / no notification needed (transparent; the
  handler's replay-cached response answers). Gate state: `Cleared`.
- `mark_failed`/TTL — reject path as below.

Semantics this fixes deliberately: the canonical identity cannot distinguish
an intentional identical call from a retry (same client + same params = same
id, by CEP-8 design). Policy: **one payment buys all invocations of one
canonical identity while the result is cached** — paid tools are idempotent
per canonical id (a second identical download is served from cache; distinct
work requires distinct params). The consumer's result store re-keys from
`idempotencyKey` to `canonicalInvocationId` (delivered in every
`PaymentGateRequest`).

**Transparent** (famulus parity, no client opt-in needed):
1. Gate matches request against priced capabilities → computes canonical
   identity → no grant → **parks** the message (holds `Next`, starts TTL timer
   capped per the route budget below), records `AwaitingInvoice`, and emits
   **one** `PaymentGateRequest` to the foreign queue.
2. Kotlin quotes (its own `QuoteCalculator`), creates the invoice, calls
   `submit_invoice(event_id, amount, pay_req, pmi, ttl)`.
3. Gate records `InvoiceIssued` and emits `notifications/payment_required`
   (amount, pay_req, pmi, ttl).
4. Kotlin polls its wallet; on settle calls `mark_settled(pay_req, meta)`.
5. Gate emits `notifications/payment_accepted` (amount, pmi, `meta` —
   `PaymentAcceptedParams`), stores the grant, immediately claims it, and only
   then **forwards the parked message**; the handler executes; the normal
   response path answers the still-open request. Ordering is test-pinned:
   accepted-notification strictly before the final response. On
   `mark_failed` / TTL expiry: `notifications/payment_rejected`, drop the
   parked message (route popped by the chain's cleanup).

**Explicit gating** (client requested it, policy `Optional`):
1. Same match → no grant → `AwaitingInvoice` + one foreign request. After
   Kotlin submits, gate answers the original request with a targeted
   `-32042 Payment Required` response (`payment_options[]` with the submitted
   invoice) and drops the message. Route survives until cleanup.
2. Retries with the same canonical identity (new event id, `_meta` regenerated
   — the canonical hash ignores it): pending (either sub-state) → targeted
   `-32043 Payment Pending` + `retry_after`; granted → `claim` (single-use,
   atomic in `AuthorizationStore`) → forward to handler → normal response.
3. `mark_settled` converts pending → granted with TTL. No parked message in
   this mode. **No `payment_accepted`/`payment_rejected` notifications** —
   the SDK classifies those params as transparent-lifecycle types; an
   explicit-gating client learns settlement through retry → claim → result
   (and `-32043` while pending), which is the CEP-8 shape. The gate emits
   accepted/rejected notifications **only in transparent mode**.

**Duplicates (pre- and post-invoice, distinct because of the window between
the gate event and `submit_invoice`):**
- `AwaitingInvoice`: no `pay_req` exists yet — coalesce: silently drop the
  duplicate (transparent; its route pops, the client's copy times out while
  the original stays open) / targeted `-32043` (gating). Exactly one foreign
  request is ever emitted per pending identity.
- `InvoiceIssued`: re-emit `notifications/payment_required` with the
  **same** `pay_req` (transparent) / `-32043` (gating), drop the duplicate.
- Never forward two parked messages for one settlement — only the oldest
  parked `Next` per canonical identity is forwarded on settle.

**Route budget (hard constraint).** Holding `Next` delays middleware cleanup
but does **not** pin the route: two independent reapers can remove it — the
route sweep at `request_timeout_secs` **and** inactive-session cleanup, which
drops a session's routes at `session_timeout_secs` (FFI defaults: 60 s and
300 s respectively, the latter equal to the default payment TTL — a real
race). A 300 s park can settle, execute, and then fail to deliver the
response on a swept route. Two enforcement points:

```
start():        request_timeout_secs and session_timeout_secs
                both > payment_ttl_cap + execution_budget + margin
submit_invoice: submitted ttl + execution_budget + margin
                < min(request_timeout_secs, session_timeout_secs)
```

- `start()` can only validate against the **cap** (the max TTL the operator
  will allow, default 300 s = `DEFAULT_PAYMENT_TTL_MS`); the per-invoice value
  arrives later, so the binding check lives in `submit_invoice` — a submitted
  TTL that breaks the invariant is rejected there with `FfiError::Validation`.
- Violations are errors, not warnings.
- FFI defaults when payments are enabled: `request_timeout_secs` ≈
  `payment_ttl_cap + execution_budget + margin`, `session_timeout_secs` set
  above that (both applied in `start()` — which can, because D4 defers
  transport construction there; consumers who set explicit values get
  validation instead of a bump).
- The parked-message TTL timer uses the **submitted** invoice TTL.
- `payment_ttl_cap`, `execution_budget`: plain values, defaults 300 s / 600 s.
- **`request_timeout_secs` and `session_timeout_secs` become `Option<u64>`**
  on the UniFFI `ServerConfig` record (breaking, same release as the split):
  `None` = auto — the FFI derives the value in `start()` when payments are
  enabled and keeps today's default otherwise; `Some(v)` = explicit — never
  bumped, only validated (error if undersized). A plain `u64` cannot
  distinguish "explicitly 60" from "default 60", which the bump-vs-validate
  rule needs.
- Gating mode has no parked message, but the same bound protects the
  retry→claim→execute window; apply it uniformly.
- `execution_budget` is **consumer-authoritative**: `Next::run` returns when
  the message is enqueued, so the gate cannot observe handler completion, and
  a gate-side timer would race the handler's own response for the route. The
  consumer's hard tool timeout (process/coroutine deadline sized to
  `execution_budget`, enforced around `Tool.execute` + `sendResponse`) is the
  enforcement point; the gate's copy exists only so the budget arithmetic is
  consistent and the parked window is provably within the route lifetime. If
  the consumer timeout fails, the route sweep is the backstop (response lost,
  client retry hits the replay path). `ponytail:` upgrading to an FFI-side
  completion timer means hooking `Server::send_response` to cancel it and a
  race test proving exactly one of timeout/handler response consumes the
  route — do it only if a consumer ships without a reliable tool timeout.

### D4 — Config-before-start via constructor/start split

UniFFI `Server` becomes two-phase (breaking change; cvm-worker is the only
consumer and migrates in the same release):

```
Server::new(keys, config)        // stores keys + config only; NOTHING built
  .set_announcement_extra_tags() // existing
  .set_priced_capabilities_json()// new: structured pricing, derives `cap` tags
  .set_payment_interaction_policy(PaymentInteractionPolicy::Optional) // new
  .start()                       // construct transport (config consumed HERE,
                                 //  with budget bumps applied), start(),
                                 //  spawn_discoverability_publication()
```

**Transport construction is deferred to `start()`.** The SDK consumes its
config in `NostrServerTransport::new(keys, sdk_config)` and exposes no
timeout setters on a live transport — a `new()` that constructed eagerly
would make the route-budget bump in `start()` impossible. `Server::new`
therefore only validates and stores; `start()` derives payment-adjusted
timeouts, builds the transport, the gate, and the
`targeted_response_sender` (after all tag setters have run — the SDK's
"sender-after-tags" ordering constraint is satisfied by construction), then
starts. Registration that the SDK only documents (`add_inbound_middleware`
before `start`, `set_supported_payment_interaction` before `start`) happens
solely inside `start()`, so a consumer cannot get the order wrong — and the
wrapper state machine below makes late/repeated use a hard error in release
builds, not a skipped `debug_assert!`.

The wrapper carries a real state machine:
`AtomicU8 ∈ {Configuring, Started, Closed}`, checked in **release** builds:
late setters → error, second `start()` → error, `recv*`/payment calls before
`start()` or after `close()` → error rather than blocking forever on a
receiver that does not exist yet. The state errors get **new `ErrorCode`
variants appended** — `NotStarted = 9`, `Closed = 10` — rather than aliasing
`Other`/`Validation` (foreign callers branch on these; reusing a mismatched
semantic is worse than two appended constants). Appending is ABI-legal per
the repo's stability rules; `headers/contextvm.h` + `c-tests` gain the two
codes in the same change. (Upstreaming release-grade guards into the SDK is
a nice-to-have PR, not a dependency.)

## Foreign (UniFFI) API surface

```kotlin
// ── config, before start() ─────────────────────────────────────────
enum class PaymentInteractionPolicy { OPTIONAL, TRANSPARENT }

server.setPaymentInteractionPolicy(policy)
server.setPricedCapabilitiesJson("""
  [{"method":"tools/call","name":"download_media",
    "amount":1000,"maxAmount":50000,"currencyUnit":"sats",
    "description":"per-invocation media download"}]""")
//   ↑ also derives and installs the announcement `cap` pricing tags
//     (SDK: cap_tags_from_priced_capabilities) — one source of truth.

// ── runtime ────────────────────────────────────────────────────────
// Drained alongside the request loop:
val preq = server.recvPaymentGateRequest(timeoutSecs)
// preq: requestEventId, clientPubkey, method, paramsJson (string),
//       capabilityName, canonicalInvocationId
// Kotlin: quote from paramsJson, wallet.createInvoice(...), then:

// amount is in the capability's advertised currencyUnit (whole sats) and MUST
// be the actual post-rounding invoice amount (see the unit contract below).
server.submitInvoice(requestEventId, amount, payReq, pmi, ttlSecs, description?)
server.markPaymentSettled(payReq, metaJson?)
server.markPaymentFailed(payReq, message?)
server.markReplayed(requestEventId)   // result cache hit: forward free, no invoice
```

**Unit contract.** One boundary unit: **whole sats**, defined as the matched
`PricedCapability.currencyUnit` (CEP-8 amounts are in the advertised unit —
cvm-worker's `QuoteCalculator` produces msat and Spark rounds msat up to whole
sats, reporting the rounded amount back). Rules:

- `set_priced_capabilities_json` enforces at parse time:
  `currencyUnit == "sats"` (the only unit the boundary implements — reject
  anything else rather than misinterpret it), `amount > 0`, and
  `maxAmount ≥ amount` when present.
- Kotlin converts msat → sats with the same ceiling division Spark uses
  (`(msat + 999) / 1000`) and submits the **actual** invoice amount, never the
  pre-rounding quote.
- `submit_invoice` validates `capability.amount ≤ amount ≤
  capability.max_amount` (when set) — the advertised `cap` range is a
  protocol promise, not decoration. Violations return
  `FfiError { code: Validation }` directly: this is FFI-boundary validation,
  and the SDK's `PaymentError` has no validation variant (it is
  `#[non_exhaustive]`; add `InvalidAmount` upstream only if the SDK itself
  ever needs to raise it — the FFI does not).
- Everything emitted on the wire (`payment_required`, `payment_accepted`,
  `-32042` options) carries the submitted amount, so client and announcement
  can never disagree.
- msat precision is deliberately not modeled — `i64` sats only.

**PMI discovery.** The gate knows the PMIs it can settle:

- Supported list: `"bitcoin-lightning-bolt11"` only
  (`PMI_BITCOIN_LIGHTNING_BOLT11`). No setter — a list of one is a constant;
  add the setter when a second PMI exists.
- `start()` derives the server's `pmi` announcement tags from that list
  (SDK: `payments::tags::pmi_tags`) and merges them into the announcement,
  alongside the `cap` tags — so discovery tells clients what the gate accepts.
- `submit_invoice` rejects a submitted `pmi` not in the list with
  `FfiError { code: Validation }`.

Errors surface as the existing `ErrorCode::Payment` (bridge already maps
`Error::Payment`; no new error code needed). All fat payloads cross as JSON
strings — the FFI's established idiom (`publish_tools`, pricing tags).

Registration of the gate is implied: a non-empty priced-capability list +
`start()` installs the middleware. No `enable_payments()` flag to forget.

## Phases

### Phase 1 — lifecycle split + negotiation config (SDK surface that exists today)

- `uniffi_types.rs`: `Server::new` stores keys + config only (transport
  construction deferred to `start()`); every pre-start setter (tags, policy,
  priced capabilities, budgets) mutates the **stored pending configuration** —
  there is no transport to forward to — and `start()` applies the accumulated
  configuration to the freshly constructed transport. `start()` also applies
  budget bumps, builds the gate, starts. Mirror enum
  `PaymentInteractionPolicy`. Wrapper state machine `Configuring/Started/
 Closed` enforced in release; `ErrorCode` gains `NotStarted`/`Closed`.
- Tests: construct → configure → start roundtrip; explicit-gating client
  against `Transparent` policy gets `-32602`; against `Optional` gets the
  effective-mode disclosure (pattern: `tests/payments_negotiation_e2e.rs`,
  needs `contextvm-sdk` with `test-utils` as an FFI dev-dependency — none
  exists yet, add it).

### Phase 2 — the gate middleware (pure Rust, in-crate)

- `payment_gate.rs`: matching (method + `params.name` for `tools/call`),
  canonical identity, `AuthorizationStore` wiring, parked-`Next` registry
  (bounded, default 128, overflow → `payment_rejected` + drop), the
  `AwaitingInvoice`/`InvoiceIssued` state model, TTL timers driven by the
  submitted invoice TTL and bounded by the route budget, both lifecycles per
  D3, all wire emissions via SDK types/constants (including
  `payment_accepted`), notifications via captured
  `Arc<Mutex<NostrServerTransport>>`.
- `submit_invoice` re-bind semantics for recovery: same canonical identity +
  same `pay_req` while pending → refresh TTL (idempotent); same identity +
  different `pay_req` while an invoice is outstanding → error (double-invoice
  guard); `mark_settled` with no parked request (crash-seeding a grant) →
  stores the grant for the retrying client.
- Foreign event queue: `tokio::sync::mpsc` (bounded), drained by Phase 3.
- Unit tests driving the state machine directly (fake wallet driver pushes
  submit/settle/fail) — no relay, no foreign code: park→submit→settle→forward
  with `payment_accepted` ordered before the response; TTL expiry; duplicate
  handling in **both** sub-states; claim-once under concurrency; parked-cap
  eviction; priced-capability parse validation (`sats`/`>0`/`max ≥ min`);
  amount min/max validation; PMI allowlist rejection; submitted-TTL vs
  `min(request_timeout, session_timeout)` rejection; re-bind/double-invoice
  guard; **replay path** (`mark_replayed` forwards free, never invoices, and
  an intentional identical call after a cached result takes the same path);
  lifecycle errors (`NotStarted` before `start()`, `Closed` after `close()`);
  transparent vs gating divergence (incl. no accepted/rejected notifications
  in gating mode).

### Phase 3 — foreign surface + `start()` wiring

- UniFFI records/methods per the API sketch; build the middleware + targeted
  sender inside `start()`; `set_priced_capabilities_json` (parse, store,
  derive `cap` tags).
- Integration test (mock relay, both lifecycles, end to end through the UniFFI
  `Server` methods): client sends `tools/call` → test drains
  `recv_payment_gate_request` → `submit_invoice` → client observes
  `notifications/payment_required` (transparent) / `-32042` (gating) →
  `mark_settled` → transparent client observes `notifications/payment_accepted`
  **then** the handler result, in that order; gating client observes only the
  retry→result path (no lifecycle notifications). Assert the response is
  deliverable (route/session not swept — budget derivation held). Plus the
  `-32043` pending-retry path and a restart simulation: re-create the server,
  `start()`, client retries → gate event → re-bind the same old invoice →
  settle → retry claims the grant without a second invoice.

### Phase 4 — ship

- Full quality gate (`fmt`, `clippy -D warnings`, `test --all --all-features`,
  `doc`), CHANGELOG entries for both crates, version bump.
- Regenerate Kotlin bindings (uniffi-bindgen-cli pinned to the Cargo `uniffi`
  version) and rebuild `.so` for `arm64-v8a` + `x86_64`.
- Hand to cvm-worker (consumer migration below).

### Consumer migration (cvm-worker, tracked separately)

- `CvmServer.kt`: adopt `start()` split; set policy `Optional` + priced
  capabilities (replacing the hand-written informational pricing tags); drain
  `recvPaymentGateRequest` in the service loop; wire Spark wallet
  (`QuoteCalculator` + `createInvoice` + existing settle-poll →
  `markPaymentSettled`), with the durable-before-submit ordering above, and
  `markReplayed` for canonical ids holding a cached terminal result.
- `YtdlpTool`: drop the in-tool invoice gate; keep pure execute +
  `PaymentSessionStore` as the result-replay cache (now orthogonal to
  payment). Add the **hard execution timeout** the route budget requires —
  yt-dlp gets an explicit `--socket-timeout`/process deadline sized to the
  configured `execution_budget` (none exists today). Optional: advertise
  `["payment_interaction","explicit_gating"]` in extra tags — allowed only
  under `Optional` policy (SDK invariant).

### Restart model (crash-safe, no double invoicing)

Rust owns payment lifecycle but its pending/granted state is in-memory and
dies with the process — after a restart Rust has **no pay_req → canonical
identity binding at all**, and the wrapper lifecycle rejects payment calls
before `start()`. So recovery cannot begin at startup: it is driven by the
next gate event, in this order:

```
start()  →  client retry/re-send arrives  →  gate emits PaymentGateRequest
        →  consumer finds persisted canonical record (unsettled)
        →  submitInvoice(same pay_req)          // re-bind; no second invoice
        →  wallet already settled? markPaymentSettled immediately
           else resume the settle-poll → markPaymentSettled
        →  gate grants; the retry claims it and executes
```

- `PaymentSessionStore` gains a payment record:
  `canonicalInvocationId → { pay_req, amount, pmi, status, submittedAt }`,
  alongside the existing result records. The key is the **canonical
  invocation id** (delivered in every `PaymentGateRequest`), not the current
  `idempotencyKey` — that includes the JSON-RPC id and will not match a
  retry. **Durable-before-submit ordering is mandatory**: the record is
  committed *before* `submitInvoice` is called —
  `create invoice → synchronous persist/commit → submitInvoice`. The store's
  current `SharedPreferences.edit {}` is async (`apply()`); the payment
  record path must use `commit()` (or an equivalent synchronous write). If
  Rust publishes the invoice and the process dies before the durable commit,
  the next retry mints a second invoice — the exact failure the ordering
  rule exists to prevent. Crash-boundary tests pin both sides: die *before*
  the commit (no record ⇒ fresh invoice is correct) and die *after* the
  commit but *before* `submitInvoice` (record exists ⇒ same invoice is
  re-bound).
- The consumer's existing pre-start `reconcile(wallet)` sweep
  (`WorkerForegroundService`) stays, but only to update its **own** records /
  dashboard — it can no longer touch the gate (nothing is bound yet). The
  re-bind happens on the first gate event for that canonical id, as above.
- The parked Rust message from the pre-crash request is gone — by design; the
  client's retry (gating) or re-send (transparent) is the recovery path, and
  both claim the grant without paying again.

## Risks / open questions

- **SDK middleware arrival.** If `payments::middleware` lands consuming
  `Arc<dyn PaymentProcessor>`, the FFI gate's internals should converge on it
  (foreign API unchanged). Track; do not block.
- **Drop-after-targeted-response semantics.** The gate answers `-32042` via
  the non-consuming targeted sender, then drops the message so the chain
  runner pops the route. This matches the SDK's own middleware tests' usage,
  but confirm with SDK maintainers that this pairing is the intended one
  (vs. leaving the route to time out).
- **Parked memory.** Bounded parked-Next registry (128) + TTL is the ceiling;
  the `AuthorizationStore` LRU (5000) bounds identities independently.
  `# ponytail: fixed caps; make configurable if a real worker hits them`.
- **Session LRU eviction** drops the negotiated mode; a gated client
  renegotiates from scratch (CEP-8: no disclosure ⇒ unestablished). Grants
  survive in-memory; process restart clears them — the consumer-driven
  restart model above is the recovery path, not re-invoicing.
- **Route budget vs. parked time.** The budget rule makes the failure
  impossible at `start()`; belt-and-braces, the settle path checks the parked
  entry's TTL before forwarding (a swept route yields a logged
  `send_response` failure, not a silent hang). If real deployments need parks
  longer than any sane request timeout, the fix is SDK-level route retention
  (`send_targeted_response` already proves the route can outlive the request);
  revisit then.
- **UniFFI version pinning.** Bindings regeneration must use the CLI tag
  matching `uniffi = "0.31"` in `contextvm-ffi/Cargo.toml`; a mismatch aborts
  at import (checksums). Already repo policy; restated because Phase 4 touches
  it.
