# contextvm-ffi

FFI bindings for the ContextVM Rust SDK.

This crate exposes two binding surfaces:

- A flat C ABI in `headers/contextvm.h`
- UniFFI objects for Python, Kotlin, and Swift

Async SDK operations are driven by an internal Tokio runtime, so foreign callers
use blocking functions and do not need to manage Rust async state.

## Build

```bash
cd contextvm-ffi
cargo build --release
```

Outputs:

- Linux: `target/release/libcontextvm_ffi.so`
- macOS: `target/release/libcontextvm_ffi.dylib`
- Windows: `target/release/contextvm_ffi.dll`

## Generate UniFFI Bindings

Build the shared library first, then generate bindings from the compiled
library metadata. The bindgen version **must match** the runtime `uniffi`
version in `Cargo.toml` — UniFFI embeds a contract version and per-function
checksums into both the library and the generated file, and a mismatch aborts
at import time (`UniFFI ... mismatch`).

The bindgen CLI is not published to crates.io; install it from git, pinned to
the tag matching your runtime version (`0.31.x` here):

```bash
cd contextvm-ffi
cargo build

cargo install --git https://github.com/mozilla/uniffi-rs --tag v0.31.2 uniffi-bindgen-cli --locked

uniffi-bindgen-cli generate target/debug/libcontextvm_ffi.so \
  --library \
  --language python \
  --out-dir python/
```

Use `--language kotlin` or `--language swift` for the other supported targets.
The generated Python file expects the native library (`libcontextvm_ffi.so` /
`.dylib` / `contextvm_ffi.dll`) to sit **next to it** at import time.

## C API

Include `headers/contextvm.h` and link against `libcontextvm_ffi`.

```c
#include "contextvm.h"

CvmError *error = NULL;
CvmHandle keys = cvm_keys_generate(&error);

char *public_key = cvm_keys_public_key(keys, &error);
cvm_string_free(public_key);

cvm_keys_free(keys);
```

Errors are opaque. Use `cvm_error_code`, `cvm_error_message`, and
`cvm_error_free` to inspect and release them.

Mode fields in `CvmServerConfig` and `CvmClientConfig` are raw `int32_t`
values. Set them with the `CVM_ENCRYPTION_*` and `CVM_GIFTWRAP_*` constants;
invalid values are rejected with `CVM_VALIDATION`.

## JSON Arguments

Several parity APIs use JSON strings to represent SDK values that are not
portable C structs:

- `profile_metadata_json`: a `ProfileMetadata` JSON object.
- `*_publish_tools/resources/prompts/resource_templates`: a JSON array of MCP
  capability objects.
- `*_set_announcement_*_tags`: a JSON array of Nostr tag arrays, for example
  `[["pricing","free"]]`.

## Memory Management

Rust-owned values returned through the C ABI must be released by the caller:

- Strings: `cvm_string_free`
- Messages: `cvm_message_free`
- Incoming requests: `cvm_incoming_request_free`
- Announcement arrays: `cvm_announcements_free`
- Discovered tool arrays: `cvm_discovered_tools_free`
- Provider profile arrays: `cvm_provider_profiles_free`
- Errors: `cvm_error_free`
