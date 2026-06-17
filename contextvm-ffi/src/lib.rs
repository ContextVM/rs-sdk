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

// C ABI functions dereference caller-provided raw pointers. The safety
// contract is documented in `headers/contextvm.h`, not enforced via `unsafe`
// (C callers cannot write `unsafe` blocks), so we allow the FFI-specific lint.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod builders;
mod channel;
mod discovery;
pub mod error;
pub mod handle;
mod kv;
mod runtime;
pub mod types;
mod uniffi_types;

// Public re-exports for integration testing
pub use channel::{
    cvm_client_ch_close, cvm_client_ch_discovered_server_capabilities, cvm_client_ch_new,
    cvm_client_ch_recv, cvm_client_ch_recv_timeout, cvm_client_ch_send,
    cvm_client_ch_server_initialize_event_json, cvm_client_ch_server_supports_ephemeral_encryption,
    cvm_gateway_ch_announce, cvm_gateway_ch_announce_event_id, cvm_gateway_ch_is_active,
    cvm_gateway_ch_new, cvm_gateway_ch_recv, cvm_gateway_ch_recv_timeout,
    cvm_gateway_ch_send_response, cvm_gateway_ch_stop, cvm_proxy_ch_is_active, cvm_proxy_ch_new,
    cvm_proxy_ch_recv, cvm_proxy_ch_recv_timeout, cvm_proxy_ch_send, cvm_proxy_ch_stop,
    cvm_server_ch_announce, cvm_server_ch_announce_event_id, cvm_server_ch_broadcast_notification,
    cvm_server_ch_close, cvm_server_ch_delete_announcements, cvm_server_ch_new,
    cvm_server_ch_publish_prompts, cvm_server_ch_publish_resource_templates,
    cvm_server_ch_publish_resources, cvm_server_ch_publish_tools, cvm_server_ch_recv,
    cvm_server_ch_recv_timeout, cvm_server_ch_send_notification, cvm_server_ch_send_response,
    cvm_server_ch_set_announcement_extra_tags, cvm_server_ch_set_announcement_pricing_tags,
};
pub use error::{ErrorCode, FfiError};
pub use handle::FfiHandle;

// Re-export types module functions for integration testing
pub use types::{
    cvm_announcements_free, cvm_decrypt_nip44, cvm_discover_all_tools, cvm_discover_servers,
    cvm_discover_tools, cvm_discovered_tools_free, cvm_encrypt_nip44, cvm_error_code,
    cvm_error_free, cvm_error_message, cvm_fetch_provider_profiles, cvm_incoming_request_free,
    cvm_keys_free, cvm_keys_from_secret_key, cvm_keys_generate, cvm_keys_public_key,
    cvm_keys_secret_key, cvm_message_free, cvm_provider_profiles_free, cvm_pubkey_hex_to_npub,
    cvm_relay_pool_connect, cvm_relay_pool_disconnect, cvm_relay_pool_free, cvm_relay_pool_new,
    cvm_string_free, cvm_version,
};

// UniFFI scaffolding — must be at crate root, after all types it references.
uniffi::setup_scaffolding!();
