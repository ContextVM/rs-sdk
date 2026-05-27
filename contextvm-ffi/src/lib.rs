// ─── ContextVM FFI — Flat C API + UniFFI for Python/Swift/Kotlin ───
//
// This crate exposes:
// 1. A flat `#[no_mangle] extern "C"` surface for direct C interop
//    (Swift via C headers, Kotlin via JNI/JNA, C/C++ directly)
// 2. UniFFI proc-macro definitions for Python and as an alternative
//    to hand-written Swift/Kotlin bindings
//
// All async work is driven on an internal global tokio runtime so
// callers never need to manage an async runtime.

mod builders;
mod channel;
mod discovery;
mod error;
mod handle;
mod kv;
mod runtime;
mod types;
mod uniffi_types;

// UniFFI scaffolding — must be at crate root, after all types it references.
uniffi::setup_scaffolding!();
