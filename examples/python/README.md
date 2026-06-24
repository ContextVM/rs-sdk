# Python examples for the ContextVM FFI binding

These examples use the **UniFFI-generated** Python binding (`contextvm_ffi.py`)
plus the platform native library (`libcontextvm_ffi.so` / `.dylib` /
`contextvm_ffi.dll`). They are the Python equivalents of the Rust examples in
[`../`](../) (`discovery.rs`, `proxy.rs`).

## The one rule: `.py` and native lib must come from the same release

UniFFI embeds a **contract version** and a **per-function checksum** into both
the compiled native library and the generated `contextvm_ffi.py`. At import
time the binding recomputes them against the library and aborts with
`InternalError("UniFFI ... mismatch")` if they differ.

So you must take the native library and the Python binding from the **same
GitHub Release / same CI run**. Never mix an old `.py` with a new `.so`.

## Get the artifacts

### Option A — from a GitHub Release

Download **two** artifacts from the release page:

1. `contextvm-ffi-bindings.tar.gz` → contains `python/contextvm_ffi.py`
2. The native archive for **your platform**:
   - Linux x86_64 → `contextvm-ffi-x86_64-unknown-linux-gnu.tar.gz`
     (`contextvm-ffi/lib/libcontextvm_ffi.so`)
   - macOS Apple Silicon → `contextvm-ffi-aarch64-apple-darwin.tar.gz`
     (`contextvm-ffi/lib/libcontextvm_ffi.dylib`)
   - macOS universal → `contextvm-ffi-universal-apple-darwin.tar.gz`
   - Windows x86_64 → `contextvm-ffi-x86_64-pc-windows-msvc.zip`
     (`contextvm-ffi/lib/contextvm_ffi.dll`)

### Option B — build locally

```bash
# from the repo root
cargo build -p contextvm-ffi                      # -> target/debug/libcontextvm_ffi.so
cargo install --git https://github.com/mozilla/uniffi-rs \
    --tag v0.31.2 uniffi-bindgen-cli --locked      # bindgen matching the runtime `uniffi` version
uniffi-bindgen-cli generate target/debug/libcontextvm_ffi.so \
    --library --language python --out-dir examples/python/
```

## Lay them out side by side

The generated `contextvm_ffi.py` looks for the native library **next to itself**
(`os.path.dirname(__file__)` + `libcontextvm_ffi.so`/`.dylib`/`.dll`). Put the
two files in this directory:

```bash
# from examples/python/
cp /path/to/contextvm-ffi/lib/libcontextvm_ffi.so .
# (place contextvm_ffi.py here too, from either Option A or B)
```

The `.gitignore` in this directory keeps both of these out of version control —
they are build/release artifacts, not source.

## Run the examples

```bash
cd examples/python

# 1. Offline sanity check (no network). Run this first to confirm the install.
python3 01_offline_check.py

# 2. Discover announced servers + their tools (needs internet).
python3 discover_servers.py
python3 discover_servers.py wss://relay.damus.io wss://nos.lol

# 3. Talk to a specific server (needs internet + a known server pubkey).
python3 proxy_tools_list.py <server_pubkey_hex>
```

## API surface

The binding exposes these top-level helpers and objects:

| Category | Names |
|----------|-------|
| Functions | `version()`, `pubkey_hex_to_npub(hex)`, `make_request(id, method, params)`, `make_response(id, result)`, `make_notification(method, params)` |
| Objects | `Keys`, `RelayPool`, `Discovery`, `Server`, `Client`, `Gateway`, `Proxy` |
| Configs | `ServerConfig(...)`, `ClientConfig(...)` (keyword-only fields) |
| Enums | `EncryptionMode.{OPTIONAL,REQUIRED,DISABLED}`, `GiftWrapMode.{OPTIONAL,EPHEMERAL,PERSISTENT}` |
| Records | `JsonRpcMessage`, `IncomingRequest`, `ServerAnnouncement`, `DiscoveredTool`, `ProviderProfile`, `PeerCapabilities` |

All async SDK work runs on an internal Tokio runtime, so every method here is
**blocking** — no `async`/`await` needed on the Python side.

See `contextvm-ffi/README.md` and `contextvm-ffi/src/uniffi_types.rs` for the
full, authoritative API.
