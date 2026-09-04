# Changelog

## [Unreleased]

### Added

- CEP-8 capability pricing and payments (in progress; foundational pieces, not yet a
  usable payment flow):
  - Server-side payment-interaction negotiation and advertisement: the server transport
    now parses client `pmi` and `payment_interaction` tags, negotiates the effective
    session mode (`transparent` by default, `explicit_gating` when the server policy
    allows it via `set_supported_payment_interaction`), rejects an unsupported
    `explicit_gating` request with a JSON-RPC `-32602` error, discloses the effective
    mode on the first response, and attaches `cap` pricing tags to capability-list
    responses. The negotiated mode is read into the inbound-middleware context for the
    payment middlewares that follow.
  - Client-side payment-interaction negotiation: the client transport now advertises its
    payment methods (`pmi`) and its requested payment interaction mode
    (`payment_interaction`) on outbound requests, configurable via `with_pmis` /
    `with_payment_interaction` or the matching setters. The `pmi` tags ride every request;
    the `payment_interaction` tag is sent once per mode and re-sent only when the requested
    mode changes, so routine invocations stay clean while a mid-session change still
    reaches the server. The client also records the effective mode the server discloses,
    readable via `get_effective_payment_interaction`, and deliberately ignores an inbound
    `payment_interaction` tag when it did not itself request `explicit_gating`, since such
    a tag is a server availability advertisement rather than this session's negotiated
    mode.
  - A targeted server response sender: `send_targeted_response` publishes a JSON-RPC
    response to a specific client and request event without consuming the request's
    correlation route, so a transport-level gate can answer a request without ending
    it. It composes the same tags every other server response carries (discovery
    replay on the first response, the CEP-8 effective-mode disclosure, and `cap`
    pricing on capability-list results), mirrors the inbound gift-wrap kind, and
    no-ops with a warning when the client has no session. `targeted_response_sender`
    returns the same publish as an injectable closure for callers that have no
    `&self`, such as a detached middleware. That closure captures the announcement
    tag sets when it is built, so build it after those tags are set.
    Both `send_targeted_response` and the `TargetedResponseSender` alias are
    re-exported from `contextvm_sdk::transport`.
  - Transparent-lifecycle payment gating on the server transport: a priced invocation now
    triggers `notifications/payment_required` and reaches the MCP handler only after the
    configured payment processor verifies settlement, with `notifications/payment_accepted`
    sent on the way. A dynamic pricing callback can reject the invocation (emitting
    `notifications/payment_rejected`) or waive payment (forwarding it untouched). Duplicate
    deliveries of the same request event share one payment rather than charging twice, without
    costing the paying request its stream: the duplicate is dropped, and the seam's
    drop-cleanup leaves the open-stream writer of a request that already reached the handler
    in place. Once an invoice has been issued a later failure never re-charges a client who
    already paid: a verified payment is delivered even if the acceptance notification cannot
    be published. Because a payment can outlast the 60 s stale-route sweep, the transport now
    captures the request's routing fields when it emits `payment_required` and delivers the
    eventual result from that capture when the route is gone.
    `payment_notification_sender` returns the payment-notification publish as an injectable
    closure for a detached middleware, alongside the existing `targeted_response_sender`.
    The middleware is not registered by default; registration arrives with the payments
    configuration entry point.
    Setting `max_pending_payments` to zero refuses every priced request, rather than running
    the server's payments through room for one.
  - Explicit-gating payment lifecycle on the server transport: in an `explicit_gating`
    session a priced invocation is now answered immediately with a JSON-RPC `-32042`
    Payment Required error carrying one payment option, and the request is dropped
    rather than held. A detached task verifies the payment and records a single-use
    grant in the authorization store, keyed by canonical invocation identity; a repeat
    invocation during verification draws `-32043` Payment Pending, and a later retry
    with the same method and params claims the grant atomically and reaches the MCP
    handler. A pricing-callback rejection answers `-32000` with the policy message; a
    waiver forwards for free. The store gains one composed operation,
    `claim_or_set_pending` (returning the new `ClaimOrPending`), which checks the grant
    and the pending slot in a single critical section so a settlement landing between
    the two checks can never mint a second invoice against an unclaimed paid grant.
    Registration arrives with the payments configuration entry point, alongside the
    transparent middleware's.
  - The server payments registration entry point:
    `contextvm_sdk::payments::with_server_payments(&mut transport, options)` composes
    the whole server-side payment stack in one call, before `start()`. It advertises
    the configured payment methods and prices on announcements (`pmi` and `cap` tags,
    plus a `payment_interaction=explicit_gating` availability tag when the policy is
    `Optional`), records the negotiation policy, and registers the transparent payment
    middleware plus, under `Optional`, the explicit-gating middleware with a fresh
    authorization store, with the payment senders built after the tags so their
    captured discovery replay is complete. It refuses (with an error, before touching
    any state) the two calls that would silently lose money: registration after
    `start()`, which would advertise payments while gating nothing, and a second
    registration, which would charge every priced request twice. The gateway joins in:
    `GatewayConfig` gains `payment_options` (with a `with_payment_options` builder),
    consumed when the gateway builds its transport in both `NostrMCPGateway::new` and
    `serve_handler`.
  - The client payments registration entry point:
    `contextvm_sdk::payments::with_client_payments(&mut transport, options)` composes
    the whole client-side payment stack in one call, before `start()`. It advertises
    the configured handlers' payment methods (replacing any config-seeded list, as the
    reference implementation does), optionally requests a payment interaction mode,
    and installs the payment engine into the transport's inbound path. The engine
    drives both lifecycles: transparent auto-pay (an in-flight dedup per payment
    request, a policy gate and the handler's own gate ahead of every wallet action,
    synthesized decline and rejection errors toward the local consumer, a
    synthetic-progress heartbeat carrying the request's original progress token, and a
    keep-alive touch loop so a slow payment survives the transport's correlation
    retention sweep) and explicit gating (`-32042` intercepted and answered through an
    `on_payment_required` callback with a byte-true retry of the cached original
    request, `-32043` retried under a capped exponential backoff seeded by the
    server's `retry_after` and floored at one second, and a fixed, documented set of
    give-up surfaces as the only paths either code reaches the consumer). It refuses,
    with an error and before touching any state, registration on a started, closed, or
    already-registered transport. The proxy joins in: `ProxyConfig` gains
    `payment_options` (with a `with_payment_options` builder), consumed when the proxy
    builds its transport in both `NostrMCPProxy::new` and `serve_client_handler`. The
    payments guide (`docs/payments.md`) documents both lifecycles end to end, the
    client shapes, and the operational notes for auto-pay deployments.

### Fixed

- Client transport: the response-correlation entry was consumed by the FIRST correlated
  inbound message of any kind, so a server that emitted a correlated notification before
  its response (a CEP-8 payment notification, or any custom correlated notification) cost
  the client that response, which was then dropped as a response for an unknown request.
  Only a response or an error response consumes the entry now; correlated notifications
  are forwarded without touching it, matching the reference implementation's
  classification by JSON-RPC type.

- Server transport: a request dropped by the inbound middleware chain (a gated priced
  invocation, a rejected payment, a chain panic) leaked its CEP-19 wrap-kind tracking
  entry. On the primary request path the entry lingered until session cleanup; on the
  CEP-22 oversized re-inject path nothing ever reclaimed it. The seam's drop-cleanup
  now releases the entry on both paths, and only when its own route pop confirms no
  concurrent responder owns the event, so response wrap-kind mirroring is never
  degraded by the release.

- rmcp server worker: a single inbound message that was not an `initialize` request could
  permanently stop the server for every client. All Nostr clients are multiplexed through one
  rmcp service, and rmcp's pre-service handshake accepts only an `initialize` request as its
  first message; anything else (a `notifications/initialized`, any other typed notification, a
  response, or an error response) terminated the service, which cancelled the worker and closed
  the transport. Nothing recovered it: every later client hung. Reaching it needed no
  authentication, no allowlisted key, and no honest client, and an honest client could trigger it
  on itself whenever the relay delivered its `notifications/initialized` before its `initialize`,
  which Nostr does not order. The worker now satisfies the handshake itself at startup, before it
  drains any relay-sourced message, so no inbound message can reach the pre-service handshake.

- CEP-41 open-stream: a redelivery of a `tools/call` that carries a progress token no longer
  disturbs the request the first delivery forwarded. Relays can deliver the same request event
  more than once; the second delivery used to replace the writer slot with a fresh, unstarted
  writer, which left `send_response` deciding a live stream's deferral against the wrong
  writer and answering on the plain path mid-stream. The first writer now stays.

- CEP-41 open-stream: a deferred final response (one held back while a stream was
  still open) went out with routing tags only, so it carried neither the CEP-35
  discovery tags nor the CEP-8 effective-mode disclosure. A client whose first server
  event was such a response captured it as its session baseline and then reported no
  server identity and no capabilities for the rest of the session. The deferred path now
  composes its tags exactly as the normal response path does: discovery tags and the
  effective-mode disclosure behind the same one-shot latches, and `cap` pricing tags on a
  capability-list result.

### Changed

- rmcp server worker: every client now receives the handler's declared `protocolVersion` in its
  `InitializeResult`. Because the worker satisfies the pre-service handshake itself, no client's
  `initialize` runs rmcp's version negotiation any more, so the first client is answered with the
  declared version rather than an echo of the version it requested. Clients after the first
  already behaved this way, so the change makes all clients uniform, in the less conformant
  direction: the multiplexed adapter no longer negotiates the protocol version per client. A
  server announcement is unaffected for a handler that does not override the version, and
  reflects the declared version for one that does. The per-client synthetic bootstrap that used
  to inject an `initialize` ahead of a new client's first request is removed, since the handshake
  is now satisfied before any client is served.

- The server transport's two response paths (the normal path and the CEP-41 deferred
  stream path) now compose their outbound tags through a single shared function
  instead of each writing the same routing, discovery, disclosure and pricing policy
  by hand. Internal refactor with no behavior or wire-format change.
- **Breaking:** `ClientSession` and `SessionSnapshot` are now `#[non_exhaustive]`. Both
  gain fields as new CEPs land (CEP-8 adds three to each), so downstream code must
  construct `ClientSession` via `ClientSession::new` and destructure either struct with
  `..` rather than exhaustively. This matches `InboundContext`, which is already
  `#[non_exhaustive]`, and makes future field additions non-breaking.
- The server transport's inbound middleware context now carries a per-event cancellation
  token derived from the transport's shutdown token, so a middleware doing long-running
  work stops when the transport closes. `send_notification`'s body moved into a shared
  publish that both it and the new injectable sender use, so the two cannot drift. A
  gated (dropped) request now releases the open-stream slot it reserved.
- FFI bindings (`contextvm-ffi`): expose the CEP-8 payment gate through UniFFI with
  `PaymentGateRequest`, `set_priced_capabilities_json`, `set_payment_interaction_policy`,
  `recv_payment_gate_request`, `submit_invoice`, `mark_payment_settled`,
  `mark_payment_failed`, and `mark_replayed`. `Server::start()` wires the gate, derives
  the `cap`/`pmi`/`payment_interaction` announcement tags, and keeps the original
  request route alive for transparent paid requests so the handler result is delivered.

## [0.2.2] - 2026-07-29

### Added

- `ClientOpenStreamHandle::cancel(token, reason)`: a `Send`-safe way to cancel a
  CEP-41 open-stream reader session by its progress token from any task —
  including a `tokio::spawn` that drives the stream. `ToolStreamCall` is `!Sync`
  (its `result: BoxFuture` field), so `abort(&self)` can't be awaited from a
  `Send` task, forcing consumers that drive a stream in a spawned task to drop the
  call without cleaning up. `cancel` mirrors `abort` (publishes the `abort` frame
  and frees the reader-registry slot via the idempotent `abort`/`consumer_abort`
  pair) but operates on the `Sync` handle, so cloning the handle into a task and
  calling `cancel(token, reason)` works from any thread.

### Fixed

- CEP-41 open-stream (server): the writer `progress_token → event_id` index is
  now scoped by `(client_pubkey, token)`, not the bare token. The progress token
  is only unique *within* a peer — rmcp mints it from a per-peer counter, so every
  client's first stream carries token `"0"`. The old global key let two concurrent
  clients clobber each other's entry, and either one's cleanup then deleted the
  shared key, orphaning the other's still-live writer. The orphaned client's
  keepalive pings found no writer → no `pong` → `Probe timeout` on an
  otherwise-alive stream, reproducing only with ≥2 concurrent clients (hence unseen
  by single-client test suites). The fix mirrors the TS `getProgressTokenKey`
  composite and the existing per-peer reader registry; `slots` (keyed by
  `event_id`) is unaffected. The oversized-transfer reassembly was verified
  already per-peer scoped (`LruCache<sender_pubkey, …>`) and needs no change.
- CEP-41 open-stream: a reader session whose `start` frame had not yet arrived no
  longer emits a keepalive `ping`. The reader `SessionState::tick` now gates its
  idle→ping transition on `started`, matching the writer's `tick` and restoring parity
  with the TS SDK (whose idle timer arms only on `start`). Previously, a session
  registered at request-publish time would ping after `idle_timeout` without ever
  receiving `start`; on the server, a ping for a token whose writer had been disposed
  (e.g. a tool that returned without streaming) was raised as a fatal
  `Received ping frame before start` sequence error and logged as a WARN, and the
  unanswered probe then aborted the client stream. (`session.rs`)
- CEP-41 open-stream: a control frame (`ping`/`pong`/`abort`) for a token with no
  reader session is now dropped at debug level instead of raising a fatal sequence
  error. Such a frame is a teardown linger or pre-start desync, not a data-plane
  violation; data frames (`chunk`/`close`) for unknown tokens still error.
  (`registry.rs`)

## [0.2.1] - 2026-07-10

### Added

- `ClientPubkey`: the rmcp server worker now injects the caller's Nostr public
  key (hex) into every **real** inbound request's `extensions` typemap, so
  tool/resource/prompt handlers can identify their caller via
  `ctx.extensions.get::<ClientPubkey>()`. It is not injected for the transport's
  own synthetic announcement/initialization drives (which carry the
  `ANNOUNCEMENT_REQUEST_ID` sentinel as their pubkey), so handlers never observe
  a bogus caller; oversized (CEP-22) requests carry a real pubkey and are
  injected as usual.
  This closes the parity gap with the TS adapter's `extra._meta.clientPubkey`, but
  uses rmcp's typed extensions (local-only, never on the wire) instead of the
  `_meta` field, so it is always on rather than opt-in. The inbound event id is
  already reachable as the rmcp request id (`ctx.id`).
- `InboundEvent`: the rmcp server worker now also injects the **full**
  client-signed Nostr request event into `extensions`, reachable via
  `ctx.extensions.get::<InboundEvent>()`. For gift-wrapped requests this is the
  inner, signature-verified event (its `pubkey` matches `ClientPubkey` by
  construction); for plaintext requests it is the outer event; for CEP-22
  oversized requests it is the carrying `end` frame's event. This exposes
  `id`, `pubkey`, `sig`, `tags`, … — notably `sig`, which the server cannot
  reconstruct without the client's private key. Handlers that must bind a tool
  call to / store / audit the publishing event (e.g. an MLS key-package
  coordinator returning the publication event) no longer have to fabricate a
  synthetic event. Injected only for real client requests; synthetic
  transport-internal requests carry none (`get` returns `None`).
- `IncomingRequest` gained an `event: Option<nostr_sdk::Event>` field carrying
  the same event through the channel seam. The FFI mirrors (`FfiIncomingRequest`,
  the UniFFI `IncomingRequest`) intentionally do not surface it yet (no FFI
  consumer needs the raw event; mirroring `nostr_sdk::Event` + `Tags` is
  non-trivial) — the omission is documented in place.

### Fixed

- fix(open-stream): abort server writers on silent client disconnect (CEP-41).
  Server→client `OpenStreamWriter`s leaked when a client silently disappeared
  (crash/sleep/network drop) without sending `abort`. CEP-41 mandates each peer
  maintain an idle timeout and probe the other with `ping`/`pong`, but only the
  reader session ran keepalive timers; a pure producer stream (e.g. a
  subscription-style tool streaming to a client) was never probed, so a dead
  client left the writer — and any upstream producer keyed on `is_active()` —
  alive indefinitely. The writer now arms an idle window once it starts
  streaming, the server keepalive sweep probes it (mirroring the existing reader
  sweep, driven by `OpenStreamWriter::tick`), an inbound `pong` for the stream is
  routed to the writer to clear the probe (`ack_probe`), and a missing `pong`
  aborts with `"Probe timeout"` (flushing any deferred final response via the
  existing `on_abort` hook) **and evicts the dead client's session** (CEP-41
  "release local state", mirroring the TS `handleProbeTimeout`, firing the
  `SessionStore` eviction callback). Per CEP-41 only inbound frames reset the idle
  window — a successful `write()` against the relay is not liveness. Reuses the
  reader `idle_timeout_ms` / `probe_timeout_ms` knobs (one idle/probe pair per
  stream). This is the rs-sdk port of the TS SDK 0.13.8 fix.
- fix(server): verify plaintext event signatures before trusting `event.pubkey`
  for handler identity, the auth allowlist, and request correlation, mirroring
  the gift-wrap arm. The default `RelayPool` verifies inbound signatures itself,
  but `RelayPoolTrait` is public, so a custom pool that skips verification
  (e.g. `MockRelayPool`) left caller identity dependent on an undocumented pool
  assumption — and a forged pubkey could bypass the auth allowlist. This is the
  rs-sdk half of the TS identity-forgery fix (ContextVM/sdk#64, #69).

## [0.2.0] - 2026-06-24

### Added

- CEP-22: oversized payload transfer for chunking MCP messages that exceed the NIP-44 single-event size limit (~65 KB), using a transport-agnostic framing engine (start/accept/chunk/end/abort frames, SHA-256 digest verification, and out-of-order reassembly), enabled by default and negotiated through the `support_oversized_transfer` capability tag so servers only fragment to clients that advertise support (#88, #89, #91)
- CEP-22: progress-aware request timeouts and an in-flight transfer watchdog, providing per-chunk idle-timeout reset, a max-total transfer cap, and receiver-side reaping of stalled transfers, opt-in via `call_tool_with_options` and `progress_aware_options` (#92)
- CEP-17: multi-stage relay resolution with server identity parsing, relay list (NIP-65) fetching, and `fetch_events`, plus transport integration that resolves a server's preferred relays before connecting (#82, #83)
- CEP-6: expanded server announcements with full `InitializeResult` parsing in `ServerAnnouncement`, auto-publishing on `start()`, relay list publishing, and a tool and resource schema mapping table (#77, #78, #79, #81)
- CEP-23: optional server profile metadata published as a NIP-01 kind 0 event, via a new `ProfileMetadata` type, so clients see a human-friendly identity (#77, #79)
- CEP-41: open-ended streaming - a server tool emits ordered chunks back to a
  client while a request is in flight via `call_tool_stream`; the client
  consumes them as an async `Stream`; the stream supplements the final
  JSON-RPC response rather than replacing it, negotiated through the
  `support_open_stream` capability tag (#97, #98)
- CI: MSRV and feature-matrix checks (#75)
- `examples/python/`: runnable Python examples using the UniFFI binding — an
  offline install sanity check, server/tool discovery (mirrors `discovery.rs`),
  and a client `tools/list` caller (mirrors `proxy.rs`).

### Changed

- Upgraded `rmcp` from 0.16.0 to 1.8 to gain progress-aware request timeouts (#86)
- Raised the minimum supported Rust version (MSRV) from 1.70 to 1.88
- Added `sha2` and `hex` dependencies for CEP-22 payload digests
- Enabled the `missing_docs` lint, closed rustdoc coverage gaps, and added SDK documentation links and a CEP-22 oversized-transfer guide (#67, #73)
- Bumped `nostr-sdk` from `0.43` to `0.44` (pulls core `nostr` `0.44.3`). No source
  changes were required: the breaking removals in the unreleased 0.45 line
  (`NostrSigner`, `TagKind`, `EventBuilder::sign_with_keys`, `TagStandard`)
  are not yet published. The SDK pins `hex` as a direct dependency, so nostr's
  internal `hex` module removal in 0.44.0 is unaffected.
- FFI: bumped `uniffi` from `0.29` to `0.31`. This raises the embedded UniFFI
  contract version (`29` -> `30`), so the generated `contextvm_ffi.py` / Swift /
  Kotlin bindings and the native library must be taken from the same release —
  a mismatch now aborts at import time with the bumped contract id. Updated
  `.github/workflows/ffi.yml` to install `uniffi-bindgen-cli` at tag `v0.31.2`
  and invoke it as `uniffi-bindgen-cli` (renamed from `uniffi-bindgen` in 0.30).

### Fixed

- `MockRelayPool` live broadcast now respects per-subscription filters instead of echoing every event to every subscriber (#90)
- Made the oversized-transfer e2e timing tests deterministic with virtual paused time and the relay config hermetic, removing CI flakiness and a 30 s real-network discovery hang (#93, #94)

## [0.1.1] - 2026-05-08

### Added

- End-to-end happy-path integration coverage for the full in-memory SDK stack, exercising RMCP handlers through `NostrServerWorker`, `NostrServerTransport`, `MockRelayPool`, `NostrClientTransport`, and the RMCP client without requiring a live network
- New `test-utils` feature for downstream integration tests that need access to `MockRelayPool`
- Public re-export of the relay module so downstream crates can use `MockRelayPool` through the crate root when `test-utils` is enabled

### Fixed

- RMCP stateless CEP-35 requests are now bridged into the RMCP lifecycle correctly by injecting synthetic initialization for first contact, allowing stateless clients to call tools and resources without an explicit `initialize` round-trip
- Corrected crates.io metadata (repository URL, keywords, categories, homepage, documentation)

### Changed

- Enabled the `rmcp` feature by default to make the native RMCP transport integration available out of the box
- Improved public API exports for transport, relay, gateway, and proxy types to simplify downstream usage

## [0.1.0] - 2026-05-07

### Added

- Core transport layer: `NostrClientTransport` and `NostrServerTransport` over NIP-59 gift wraps
- Gateway and Proxy high-level APIs for bridging MCP over Nostr
- Discovery API: `discover_servers`, `discover_tools`, `discover_resources`, `discover_prompts`, `discover_resource_templates`
- CEP-6: server announcement publishing and querying (kinds 11316–11320)
- CEP-19: ephemeral gift wraps (kind 21059) with `GiftWrapMode` negotiation on both client and server
- CEP-35: stateless session discovery, tag composition, and capability learning
- LRU-bounded session store with configurable capacity (default 1000 sessions) and TTL expiry
- Multi-client support in `NostrServerWorker` (removed single-peer barrier)
- Direct rmcp transport adapters via `into_rmcp_transport()` for native `ContextVM` services
- `CancellationToken`-based graceful shutdown on `close()`
- TTL sweep for client and server correlation stores to prevent pending-request leaks
- `MockRelayPool` for deterministic offline testing
- Builder pattern for all transport and worker configuration structs
- Four examples: gateway, proxy, discovery, and rmcp integration test

### Fixed

- Single-peer barrier in RMCP worker rejected concurrent clients (#60)
- Pending-request leak: correlation store entries never expired by TTL (#61)
- Event loop tasks not cancelled on `close()`, causing resource leaks (#63)
- `RecvError::Lagged` killing event loop under high relay throughput (#68)
- Client race condition: responses lost when publish completed before correlation registration (#55)
- Uncorrelated responses (missing `e` tag) forwarded to consumer instead of dropped (#55)
- Non-atomic `send_response` behavior in server transport (#48)
- Unbounded LRU cache initialization with zero capacity (#50)
- Announced servers not sending JSON-RPC `-32000 Unauthorized` error for disallowed clients (#53)
