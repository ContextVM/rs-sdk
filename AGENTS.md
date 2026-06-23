# AGENTS.md

Guide for AI coding agents working on the ContextVM Rust SDK (`contextvm-sdk`)
and its FFI bindings crate (`contextvm-ffi`). Read this before making changes.

## Project Overview

`contextvm-sdk` is the Rust implementation of the **ContextVM protocol** —
**MCP (Model Context Protocol) over Nostr**. It lets MCP servers and clients
talk over the Nostr network with decentralized discovery, cryptographic
identity (NIP-06 keys, NIP-44 encryption, NIP-59 gift wrap), and optional
end-to-end encryption.

This repository is a **Cargo workspace** with two members:

| Crate | Path | Purpose |
|-------|------|---------|
| `contextvm-sdk` | `.` (root) | The SDK itself. Published to crates.io. |
| `contextvm-ffi` | `contextvm-ffi/` | FFI bindings for C / Python / Swift / Kotlin consumers. |

- **Repository**: `https://github.com/ContextVM/rs-sdk`
- **MSRV**: `1.88` (declared in `Cargo.toml` as `rust-version`)
- **License**: MIT
- **Current `contextvm-sdk` version**: see `Cargo.toml` / `CHANGELOG.md`

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    Your Application                       │
├──────────────┬───────────────┬────────────────────────────┤
│   Gateway    │     Proxy     │        Discovery           │
│  (server →   │  (nostr →     │  (find servers &           │
│    nostr)    │    client)    │   capabilities)            │
├──────────────┴───────────────┴────────────────────────────┤
│                    Transport Layer                         │
│            NostrServerTransport / NostrClientTransport     │
├───────────────────────────────────────────────────────────┤
│  Core   │  Encryption   │  Relay    │  Signer   │ Oversized │
│ (types, │ (NIP-44,      │ (pool     │ (key      │ Transfer  │
│  RPC,   │  NIP-59)      │  mgmt)    │  mgmt)    │ + OpenStr │
│ valid.) │               │           │           │           │
├─────────┴────────────────┴───────────┴───────────┴──────────┤
│                   Nostr Network (relays)                   │
└───────────────────────────────────────────────────────────┘
```

### Source layout (`src/`)

| Module | Responsibility |
|--------|----------------|
| `core/` | JSON-RPC 2.0 message types, validation, top-level `Error` enum. |
| `signer/` | Nostr key management (generate / from secret / public / secret hex). |
| `relay/` | Relay pool connection management. `MockRelayPool` lives here (behind `test-utils`). |
| `encryption/` | NIP-44 encryption + NIP-59 gift wrap (kinds `1059` / `21059`). |
| `transport/` | `NostrServerTransport` + `NostrClientTransport` (the core engines). |
| `transport/oversized_transfer/` | CEP-22 chunked reassembly for large payloads. |
| `transport/open_stream/` | CEP-41 open-stream engine (frame, session, sequencing). |
| `discovery/` | Discover servers, tools, provider profiles (kinds `11316`–`11320`). |
| `gateway/` | `NostrMCPGateway` — bridges a local MCP server onto Nostr (server side). |
| `proxy/` | `NostrMCPProxy` — bridges a local MCP client to a remote Nostr server (client side). |
| `rmcp_transport/` | Native RMCP integration (`rmcp` feature). |

### Protocol (Nostr kinds)

| Kind | Name | Type | CEP |
|------|------|------|-----|
| `25910` | ContextVM Messages | Ephemeral | base |
| `1059` | Gift Wrap (NIP-59) | Regular | — |
| `21059` | Ephemeral Gift Wrap | Ephemeral | CEP-19 |
| `10002` | Relay List Metadata | Replaceable | CEP-17 |
| `11316`–`11320` | Announcement / Tools / Resources / Templates / Prompts | Addressable | discovery |

Messages route via `p` tags (recipient) and correlate via `e` tags (request event id).
See `README.md` and `DESIGN.md` for full protocol detail; per-area docs live in `docs/`.

### Cargo features

- `default = ["rmcp"]` — native RMCP transport on by default.
- `rmcp` — enables the `rmcp` git dependency and `rmcp_transport` module.
- `test-utils` — exposes `MockRelayPool` for downstream integration tests.

## Setup & Commands

```bash
# Build everything (SDK + FFI)
cargo build

# Full quality gate (what CI runs) — MUST pass before merge
cargo fmt --all -- --check
cargo clippy --all --all-features --all-targets -- -D warnings
cargo check --all --all-features
cargo test --all --all-features
cargo doc --no-deps --all-features

# Also verify it builds/tests with rmcp disabled
cargo test --no-default-features
```

### Testing strategy

- **Unit tests** (`#[cfg(test)] mod tests` inside each module) — fast, hermetic.
- **Conformance tests** (`tests/conformance_*.rs`) — wire-format, dedup, signer,
  stateless mode, stores. Pure, no network.
- **Integration tests** (`tests/e2e_happy_path.rs`, `transport_integration.rs`,
  `oversized_timeout_e2e.rs`) — require `rmcp` + `test-utils`; run a full in-memory
  stack via `MockRelayPool` (no live network).
- **FFI tests** (`contextvm-ffi/tests/e2e_ffi_*.rs`) — exercise the FFI surface
  end-to-end through the C ABI.
- **`nak-tests` feature** (FFI): the `e2e_ffi_nak_relay` test spawns the external
  `nak` CLI relay. It is **off by default**; with `--all-features` it compiles and
  **no-ops** (stderr note, not a failure) when `nak` is absent from `PATH`. Do not
  make this test hard-fail on a missing binary.

Run a single test by name:

```bash
cargo test --all-features <test_name>
cargo test --all-features --test e2e_happy_path
```

### Examples

`examples/` contains runnable references: `discovery.rs`, `gateway.rs`,
`proxy.rs`, `rmcp_integration_test.rs`, `native_echo_server.rs`,
`native_echo_client.rs`. The last three require the `rmcp` feature.

## The FFI Subsystem (`contextvm-ffi/`)

This is the crate most likely to break in subtle ways. Understand it before
touching it.

### What it is

A **translation layer** exposing the Rust SDK to non-Rust languages. Two
independent binding surfaces over the same SDK:

1. **Hand-written C ABI** — `headers/contextvm.h` + `#[no_mangle] extern "C"`
   functions in `src/types.rs` / `src/channel.rs`. For C/C++, and Swift via a
   C module map.
2. **UniFFI proc-macros** — `src/uniffi_types.rs`. Auto-generates idiomatic
   Python / Swift / Kotlin objects.

`Cargo.toml` declares `crate-type = ["cdylib", "staticlib", "lib"]` so the same
crate produces a dynamic lib (`.so`/`.dylib`/`.dll`), a static lib
(`.a`/`.lib`), and a Rust rlib (for tests/docs).

### The three pillars

| Pillar | File | Job |
|--------|------|-----|
| Global Tokio runtime | `src/runtime.rs` | One lazily-initialized `Runtime` (`OnceLock`). Foreign callers never see `async`; every FFI fn does `global_runtime().block_on(...)`. Never shut down. |
| Handle table | `src/kv.rs` | Global `HashMap<u64, Arc<dyn Any>>`. Foreign code gets opaque integer `CvmHandle { id }`; real Rust objects live in the table. |
| Error bridge | `src/error.rs` | Maps the SDK's rich `Error` enum → 9 flat `CvmErrorCode` ints. |

### THE maintenance contract — read this

**The FFI is a living contract that must track the SDK.** The canonical
incident: CEP-41 added `contextvm_sdk::Error::OpenStream`; the FFI's `match` in
`src/error.rs` didn't cover it; CI broke (`error[E0004]: non-exhaustive
patterns`). This is a recurring pattern, not a one-off.

**The explicit, non-wildcard `match` is intentional** — it forces a compile
error when the SDK adds an `Error` variant, so a new variant can never silently
fall through to `Other`. Do NOT replace it with a `_ =>` wildcard.

**When the SDK changes, check these three sync points:**

1. **`src/error.rs`** — `ErrorCode::from(&Error)`. Add a match arm for any new
   `Error` variant and map it to the right `CvmErrorCode` (add a new `CVM_*`
   constant in `headers/contextvm.h` only if warranted).
2. **`src/uniffi_types.rs`** — every `Record`/`Enum`/`Object` mirrors an SDK
   type. If the SDK type gains a field, the mirror needs it or data is dropped.
3. **`headers/contextvm.h`** — hand-maintained. Adding/renameing/reordering an
   `extern "C"` fn or changing a struct requires editing the header to match,
   or C consumers get silent memory corruption.

### ABI stability rules

These are part of the public ABI; **never** change them in a patch release:

- `CvmErrorCode` numeric values (`CVM_TRANSPORT = 1`, …). Only **append** new
  codes; never renumber.
- Struct field **order** in `contextvm.h`. Only append fields; never reorder.
- Function signatures. Adding functions is fine; changing/removing is breaking.

### FFI memory model

- Foreign callers MUST free every Rust-owned thing returned through the C ABI
  with the matching `cvm_*_free` function (`cvm_string_free`, `cvm_message_free`,
  `cvm_error_free`, `cvm_keys_free`, etc.). The free functions are null-safe.
- The handle table grows unboundedly if callers don't free. This is per-call
  leakage, not catastrophic — but the free requirement must stay documented.
- UniFFI consumers (Python/Swift/Kotlin) get automatic memory management.

### FFI build & test

```bash
# Build the bindings library
cargo build --release -p contextvm-ffi

# Generate UniFFI language bindings (needs uniffi-bindgen-cli 0.29)
uniffi-bindgen generate target/debug/libcontextvm_ffi.so \
  --library --language python --out-dir python/   # or kotlin / swift

# C test suite — verifies the hand-written header matches Rust signatures
cd contextvm-ffi/c-tests && make test
```

The C test suite (`contextvm-ffi/c-tests/test_ffi.c`) is the **only** check that
`contextvm.h` stays in sync with the Rust `extern "C"` surface — keep it in CI
and extend it when you add C functions.

## Code Style

- Rust 2021 edition, MSRV 1.88.
- `cargo fmt` formatting is enforced (CI fails on drift).
- `cargo clippy --all --all-features -- -D warnings` is enforced (zero warnings).
- Public items require doc comments (`#![warn(missing_docs)]` is in effect for
  the FFI crate; keep new public SDK items documented too).
- Tests are required for new functionality — add or update tests even if not
  asked.

## CI / Release

Workflows live in `.github/workflows/`:

- **`ci.yml`** — quality gates (fmt, clippy, check, test all-features,
  no-default-features, doc, rmcp integration example, MSRV 1.88). Runs on
  push/PR to `main` and on `v*` tags.
- **`ffi.yml`** — FFI packaging. Builds per-platform native libraries
  (linux x86_64, macOS arm64/x86_64/universal, Windows x86_64), runs the C
  test suite, generates UniFFI bindings, and on `v*` tags uploads archives to
  the GitHub Release. Release archives bundle the native lib + static lib +
  `contextvm.h` + README per target; bindings ship as a separate archive.

**Releasing**: push a `v*` tag. The FFI workflow assembles and uploads
artifacts automatically; `ci.yml` + `ffi.yml` must both be green.

## Pull Request Guidelines

- Run the full quality gate before pushing:
  `cargo fmt --all -- --check && cargo clippy --all --all-features --all-targets -- -D warnings && cargo test --all --all-features`
- If you touched `src/core/error.rs` or the SDK error enum, **also** update
  `contextvm-ffi/src/error.rs` and verify the FFI still compiles
  (`cargo check --all --all-features`).
- If you touched an SDK type exposed via FFI, update `src/uniffi_types.rs` and
  `contextvm-ffi/headers/contextvm.h`, and extend `c-tests/test_ffi.c`.
- Commit messages: conventional-style (`feat:`, `fix:`, `docs:`, `chore:`,
  `refactor:`) with a scope when helpful (e.g. `fix(ffi): …`).
- Update `CHANGELOG.md` for user-visible changes.

## Gotchas

- **Non-exhaustive `Error` match in the FFI is intentional** — it makes SDK
  additions a loud compile error instead of silent misclassification. Add the
  arm; don't wildcard.
- **`headers/contextvm.h` is hand-written** and drifts silently. The C test
  suite is the safety net — never disable it.
- **`recv_try` returns `Ok(None)` on mutex contention**, not only on empty — a
  deliberate non-blocking semantic (see `src/uniffi_types.rs`).
- **The global FFI runtime never shuts down** (intentional). Fine for long-lived
  hosts; embeds in short-lived CLIs pay a few MB of resident threads.
- **MSRV job regenerates `Cargo.lock`** — don't commit dependency bumps that
  require a newer compiler without bumping `rust-version`.
- **The `sdk/` path** (if present) is a sibling reference used during some
  cross-repo work and is not part of this crate's build graph; ignore it unless
  explicitly working on it.
