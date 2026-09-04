# Changelog

## [Unreleased]

### Fixed

- Payment gate no longer allows settled or replayed invocations to be reused as free
  authorizations. `mark_payment_settled` (transparent) and the gating retry path remove
  the local parked entry once the request is forwarded. `mark_replayed` forwards the
  current request and removes the entry rather than leaving a persistent `Replayed` state.
- `mark_replayed` now forwards the current request in both transparent and gating modes
  and no longer discards the `Next` continuation early in gating mode.
- Parked registry capacity check, eviction, and insert are now performed atomically under
  a single `parking` lock, and every exit path (settle, fail, expire, evict, queue
  overflow, discard) fully releases the parked `Next` continuation.
- `Server::start()` now uses a `Starting` state and abandons if `close()` wins the race
  while the transport is being built, preventing a closed server from being resurrected.
- `Some(0)` for `request_timeout_secs` or `session_timeout_secs` is now a validation
  error instead of falling through to defaults.
- `-32042` and `-32043` FFI error payloads are now byte-identical to the SDK builders by
  reusing `build_payment_required_error` and `build_payment_pending_error`.
- Removed the dead `SUPPORTED_PAYMENT_METHOD_IDS` constant; the gate validates against the
  SDK's `PMI_BITCOIN_LIGHTNING_BOLT11` constant.

### Added

- `IncomingRequest` (UniFFI and C) now carries an optional `canonical_invocation_id` so
  consumers can key their payment result cache by canonical identity.

### Changed

- **Breaking (UniFFI Kotlin/Swift):** `Server` and `Client` lifecycle methods are now
  exported as `shutdown()` instead of `close()`. The `close()` name conflicts with the
  `AutoCloseable` method that UniFFI generates for every object in recent binders.
  C consumers using `cvm_server_ch_close` / `cvm_client_ch_close` are unaffected.

## [0.2.0] - 2026-09-04

### Added

- CEP-8 payment gate middleware (`src/payment_gate.rs`) for priced `tools/call` invocations.
  The gate parks paid requests, emits `PaymentGateRequest` events to the foreign consumer,
  and supports both the transparent lifecycle (`payment_required` / `payment_accepted` /
  `payment_rejected` notifications) and the explicit-gating lifecycle (targeted `-32042`
  Payment Required and `-32043` Payment Pending responses, with single-use grant replay).
- UniFFI payment surface on `Server`:
  - `PaymentInteractionPolicy` enum (`Optional` / `Transparent`) and
    `set_payment_interaction_policy`.
  - `set_priced_capabilities_json` to register priced capabilities, validate the
    `sats`-only currency unit and `maxAmount >= amount`, and derive the announcement
    `cap` pricing tags.
  - `recv_payment_gate_request`, `submit_invoice`, `mark_payment_settled`,
    `mark_payment_failed`, and `mark_replayed` for foreign wallet integration.
  - `PaymentGateRequest` record carries `requestEventId`, `clientPubkey`, `method`,
    `paramsJson`, `capabilityName`, and `canonicalInvocationId`.
- New FFI `ErrorCode` variants: `NotStarted = 9` and `Closed = 10`, surfaced by the
  UniFFI `Server` and C error bridge when an operation requires a started/closed server.
- Payment-aware timeout derivation in `Server::start()`: when payments are enabled,
  `request_timeout_secs` and `session_timeout_secs` are auto-sized to fit the payment
  TTL cap, execution budget, and a route-budget margin.

### Changed

- **Breaking:** UniFFI `Server` lifecycle is now two-phase. `Server::new(keys, config)` only
  stores the configuration; `Server::start()` builds the transport, applies the accumulated
  pre-start configuration, and begins listening. Pre-start setters (`set_announcement_extra_tags`,
  `set_announcement_pricing_tags`, `set_priced_capabilities_json`, `set_payment_interaction_policy`)
  must be called before `start()` and are enforced by a release-build state machine
  (`Configuring` → `Started` → `Closed`).
- **Breaking:** `ServerConfig.request_timeout_secs` and `ServerConfig.session_timeout_secs`
  are now `Option<u64>`. `None` lets `start()` derive payment-aware defaults (or keep the
  SDK defaults when payments are disabled). `Some(v)` uses the explicit value and is
  validated against the route-budget lower bound when payments are enabled; it is never
  silently bumped.
- `Server::close()` transitions the server to the `Closed` state and rejects subsequent
  runtime and payment operations with `ErrorCode::Closed` instead of blocking or panicking.
