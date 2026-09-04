# Changelog

## [Unreleased]

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
