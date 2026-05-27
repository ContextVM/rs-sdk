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
library metadata using `uniffi-bindgen` 0.29.x:

```bash
cd contextvm-ffi
cargo build

uniffi-bindgen generate target/debug/libcontextvm_ffi.so \
  --library \
  --crate contextvm_ffi \
  --language python \
  --out-dir python/
```

Use `--language kotlin` or `--language swift` for the other supported targets.

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

## Memory Management

Rust-owned values returned through the C ABI must be released by the caller:

- Strings: `cvm_string_free`
- Messages: `cvm_message_free`
- Incoming requests: `cvm_incoming_request_free`
- Announcement arrays: `cvm_announcements_free`
- Discovered tool arrays: `cvm_discovered_tools_free`
- Provider profile arrays: `cvm_provider_profiles_free`
- Errors: `cvm_error_free`
