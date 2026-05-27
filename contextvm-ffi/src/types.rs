//! FFI-exposed types and flat C API functions.
//!
//! This module defines the FFI-safe struct mirrors, `#[no_mangle] extern "C"`
//! functions for key management, relay pool, discovery, encryption, and utility
//! operations.  The channel-based transport APIs live in `channel.rs`.

use crate::error::{set_error, ErrorCode, FfiError};
use crate::handle::FfiHandle;
use crate::kv;
use crate::runtime::global_runtime;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

// ─── FFI-safe struct mirrors ───────────────────────────────────────────

/// Encryption mode.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    Optional = 0,
    Required = 1,
    Disabled = 2,
}

impl From<EncryptionMode> for contextvm_sdk::EncryptionMode {
    fn from(m: EncryptionMode) -> Self {
        match m {
            EncryptionMode::Optional => contextvm_sdk::EncryptionMode::Optional,
            EncryptionMode::Required => contextvm_sdk::EncryptionMode::Required,
            EncryptionMode::Disabled => contextvm_sdk::EncryptionMode::Disabled,
        }
    }
}

/// Gift-wrap mode (CEP-19).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiftWrapMode {
    Optional = 0,
    Ephemeral = 1,
    Persistent = 2,
}

impl From<GiftWrapMode> for contextvm_sdk::GiftWrapMode {
    fn from(m: GiftWrapMode) -> Self {
        match m {
            GiftWrapMode::Optional => contextvm_sdk::GiftWrapMode::Optional,
            GiftWrapMode::Ephemeral => contextvm_sdk::GiftWrapMode::Ephemeral,
            GiftWrapMode::Persistent => contextvm_sdk::GiftWrapMode::Persistent,
        }
    }
}

/// JSON-RPC message type discriminator.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonRpcType {
    Request = 0,
    Response = 1,
    ErrorResponse = 2,
    Notification = 3,
}

/// An FFI-safe JSON-RPC message.
#[repr(C)]
#[derive(Debug)]
pub struct FfiJsonRpcMessage {
    pub msg_type: JsonRpcType,
    pub payload_json: *mut c_char,
    pub method: *mut c_char,
    pub id: *mut c_char,
}

/// An FFI-safe incoming request (server-side).
#[repr(C)]
#[derive(Debug)]
pub struct FfiIncomingRequest {
    pub message: FfiJsonRpcMessage,
    pub client_pubkey: *mut c_char,
    pub event_id: *mut c_char,
    pub is_encrypted: bool,
}

/// A discovered server announcement.
#[repr(C)]
#[derive(Debug)]
pub struct FfiServerAnnouncement {
    pub pubkey: *mut c_char,
    pub name: *mut c_char,
    pub version: *mut c_char,
    pub picture: *mut c_char,
    pub about: *mut c_char,
    pub website: *mut c_char,
    pub event_id: *mut c_char,
}

/// A discovered MCP tool and provider metadata.
#[repr(C)]
#[derive(Debug)]
pub struct FfiDiscoveredTool {
    pub provider_pubkey: *mut c_char,
    pub provider_display_name: *mut c_char,
    pub provider_name: *mut c_char,
    pub provider_about: *mut c_char,
    pub provider_picture: *mut c_char,
    pub provider_nip05: *mut c_char,
    pub tool_name: *mut c_char,
    pub description: *mut c_char,
    pub schema_json: *mut c_char,
}

/// Nostr profile metadata for a provider.
#[repr(C)]
#[derive(Debug)]
pub struct FfiProviderProfile {
    pub pubkey: *mut c_char,
    pub name: *mut c_char,
    pub about: *mut c_char,
    pub picture: *mut c_char,
    pub nip05: *mut c_char,
}

/// Server transport config for FFI.
#[repr(C)]
#[derive(Debug)]
pub struct FfiServerConfig {
    pub relay_urls: *mut *mut c_char,
    pub relay_url_count: usize,
    pub encryption_mode: EncryptionMode,
    pub gift_wrap_mode: GiftWrapMode,
    pub is_announced_server: bool,
    pub server_name: *mut c_char,
    pub server_version: *mut c_char,
    pub server_picture: *mut c_char,
    pub server_about: *mut c_char,
    pub server_website: *mut c_char,
    pub allowed_pubkeys: *mut *mut c_char,
    pub allowed_pubkey_count: usize,
    pub session_timeout_secs: u64,
    pub cleanup_interval_secs: u64,
}

/// Client transport config for FFI.
#[repr(C)]
#[derive(Debug)]
pub struct FfiClientConfig {
    pub relay_urls: *mut *mut c_char,
    pub relay_url_count: usize,
    pub server_pubkey: *mut c_char,
    pub encryption_mode: EncryptionMode,
    pub gift_wrap_mode: GiftWrapMode,
    pub is_stateless: bool,
    pub timeout_secs: u64,
}

// ─── Internal conversion helpers ───────────────────────────────────────

pub fn c_str_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok().map(String::from) }
}

pub fn string_to_c(s: String) -> *mut c_char {
    CString::new(s).unwrap_or_default().into_raw()
}

pub fn opt_string_to_c(s: Option<String>) -> *mut c_char {
    s.map(string_to_c).unwrap_or(ptr::null_mut())
}

pub fn c_str_array_to_vec(ptr: *mut *mut c_char, count: usize) -> Vec<String> {
    if ptr.is_null() || count == 0 {
        return Vec::new();
    }
    unsafe {
        let slice = std::slice::from_raw_parts(ptr, count);
        slice.iter().filter_map(|&p| c_str_to_string(p)).collect()
    }
}

/// Convert an SDK `JsonRpcMessage` to the FFI representation.
pub fn message_to_ffi(msg: &contextvm_sdk::JsonRpcMessage) -> FfiJsonRpcMessage {
    let json_str = serde_json::to_string(msg).unwrap_or_default();
    let msg_type = match msg {
        contextvm_sdk::JsonRpcMessage::Request(_) => JsonRpcType::Request,
        contextvm_sdk::JsonRpcMessage::Response(_) => JsonRpcType::Response,
        contextvm_sdk::JsonRpcMessage::ErrorResponse(_) => JsonRpcType::ErrorResponse,
        contextvm_sdk::JsonRpcMessage::Notification(_) => JsonRpcType::Notification,
    };
    FfiJsonRpcMessage {
        msg_type,
        payload_json: string_to_c(json_str),
        method: opt_string_to_c(msg.method().map(String::from)),
        id: opt_string_to_c(msg.id().map(|v| v.to_string())),
    }
}

// ─── Free functions ────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cvm_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            let _ = CString::from_raw(s);
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_message_free(msg: FfiJsonRpcMessage) {
    cvm_string_free(msg.payload_json);
    cvm_string_free(msg.method);
    cvm_string_free(msg.id);
}

#[no_mangle]
pub extern "C" fn cvm_incoming_request_free(req: FfiIncomingRequest) {
    cvm_message_free(req.message);
    cvm_string_free(req.client_pubkey);
    cvm_string_free(req.event_id);
}

#[no_mangle]
pub extern "C" fn cvm_announcements_free(announcements: *mut FfiServerAnnouncement, count: usize) {
    if announcements.is_null() {
        return;
    }
    unsafe {
        let slice = std::slice::from_raw_parts_mut(announcements, count);
        for ann in slice.iter_mut() {
            cvm_string_free(ann.pubkey);
            cvm_string_free(ann.name);
            cvm_string_free(ann.version);
            cvm_string_free(ann.picture);
            cvm_string_free(ann.about);
            cvm_string_free(ann.website);
            cvm_string_free(ann.event_id);
        }
        let _ = Vec::from_raw_parts(announcements, count, count);
    }
}

#[no_mangle]
pub extern "C" fn cvm_discovered_tools_free(tools: *mut FfiDiscoveredTool, count: usize) {
    if tools.is_null() {
        return;
    }
    unsafe {
        let slice = std::slice::from_raw_parts_mut(tools, count);
        for tool in slice.iter_mut() {
            cvm_string_free(tool.provider_pubkey);
            cvm_string_free(tool.provider_display_name);
            cvm_string_free(tool.provider_name);
            cvm_string_free(tool.provider_about);
            cvm_string_free(tool.provider_picture);
            cvm_string_free(tool.provider_nip05);
            cvm_string_free(tool.tool_name);
            cvm_string_free(tool.description);
            cvm_string_free(tool.schema_json);
        }
        let _ = Vec::from_raw_parts(tools, count, count);
    }
}

#[no_mangle]
pub extern "C" fn cvm_provider_profiles_free(profiles: *mut FfiProviderProfile, count: usize) {
    if profiles.is_null() {
        return;
    }
    unsafe {
        let slice = std::slice::from_raw_parts_mut(profiles, count);
        for profile in slice.iter_mut() {
            cvm_string_free(profile.pubkey);
            cvm_string_free(profile.name);
            cvm_string_free(profile.about);
            cvm_string_free(profile.picture);
            cvm_string_free(profile.nip05);
        }
        let _ = Vec::from_raw_parts(profiles, count, count);
    }
}

// ─── Signer / Key management ───────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cvm_keys_generate(_error: *mut *mut FfiError) -> FfiHandle {
    kv::insert(contextvm_sdk::signer::generate())
}

#[no_mangle]
pub extern "C" fn cvm_keys_from_secret_key(
    sk: *const c_char,
    error: *mut *mut FfiError,
) -> FfiHandle {
    let sk_str = match c_str_to_string(sk) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: "null secret key string".into(),
                },
            );
            return FfiHandle { id: 0 };
        }
    };
    match contextvm_sdk::signer::from_sk(&sk_str) {
        Ok(keys) => kv::insert(keys),
        Err(e) => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: e.to_string(),
                },
            );
            FfiHandle { id: 0 }
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_keys_public_key(handle: FfiHandle, error: *mut *mut FfiError) -> *mut c_char {
    let guard = kv::get::<contextvm_sdk::signer::Keys>(handle);
    match guard {
        Some(keys) => string_to_c(keys.public_key().to_hex()),
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid key handle".into(),
                },
            );
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_keys_secret_key(handle: FfiHandle, error: *mut *mut FfiError) -> *mut c_char {
    let guard = kv::get::<contextvm_sdk::signer::Keys>(handle);
    match guard {
        Some(keys) => string_to_c(keys.secret_key().to_secret_hex()),
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid key handle".into(),
                },
            );
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_keys_free(handle: FfiHandle) {
    kv::remove(handle);
}

// ─── Relay Pool ────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cvm_relay_pool_new(
    keys_handle: FfiHandle,
    error: *mut *mut FfiError,
) -> FfiHandle {
    let keys = match kv::get::<contextvm_sdk::signer::Keys>(keys_handle) {
        Some(k) => k.as_ref().clone(),
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid key handle".into(),
                },
            );
            return FfiHandle { id: 0 };
        }
    };
    match global_runtime().block_on(contextvm_sdk::RelayPool::new(keys)) {
        Ok(p) => kv::insert(p),
        Err(e) => {
            set_error(error, e.into());
            FfiHandle { id: 0 }
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_relay_pool_connect(
    pool_handle: FfiHandle,
    urls: *mut *mut c_char,
    url_count: usize,
    error: *mut *mut FfiError,
) -> bool {
    let guard = kv::get::<contextvm_sdk::RelayPool>(pool_handle);
    let pool = match guard {
        Some(p) => p,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid pool handle".into(),
                },
            );
            return false;
        }
    };
    let url_vec = c_str_array_to_vec(urls, url_count);
    match global_runtime().block_on(pool.connect(&url_vec)) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_relay_pool_disconnect(
    pool_handle: FfiHandle,
    error: *mut *mut FfiError,
) -> bool {
    let guard = kv::get::<contextvm_sdk::RelayPool>(pool_handle);
    let pool = match guard {
        Some(p) => p,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid pool handle".into(),
                },
            );
            return false;
        }
    };
    match global_runtime().block_on(pool.disconnect()) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_relay_pool_free(handle: FfiHandle) {
    kv::remove(handle);
}

// ─── Discovery ─────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cvm_discover_servers(
    pool_handle: FfiHandle,
    relay_urls: *mut *mut c_char,
    url_count: usize,
    out_count: *mut usize,
    error: *mut *mut FfiError,
) -> *mut FfiServerAnnouncement {
    let guard = kv::get::<contextvm_sdk::RelayPool>(pool_handle);
    let pool = match guard {
        Some(p) => p,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid pool handle".into(),
                },
            );
            return ptr::null_mut();
        }
    };
    let urls = c_str_array_to_vec(relay_urls, url_count);
    let client = pool.client();

    let result = global_runtime()
        .block_on(async { contextvm_sdk::discovery::discover_servers(client, &urls).await });

    let announcements = match result {
        Ok(a) => a,
        Err(e) => {
            set_error(error, e.into());
            return ptr::null_mut();
        }
    };

    let count = announcements.len();
    let ffi_announcements: Vec<FfiServerAnnouncement> = announcements
        .into_iter()
        .map(|a| FfiServerAnnouncement {
            pubkey: string_to_c(a.pubkey),
            name: opt_string_to_c(a.server_info.name),
            version: opt_string_to_c(a.server_info.version),
            picture: opt_string_to_c(a.server_info.picture),
            about: opt_string_to_c(a.server_info.about),
            website: opt_string_to_c(a.server_info.website),
            event_id: string_to_c(a.event_id.to_hex()),
        })
        .collect();

    unsafe {
        if !out_count.is_null() {
            *out_count = count;
        }
    }

    let mut ffi_announcements = ffi_announcements;
    let ptr = ffi_announcements.as_mut_ptr();
    std::mem::forget(ffi_announcements);
    ptr
}

#[no_mangle]
pub extern "C" fn cvm_discover_tools(
    pool_handle: FfiHandle,
    provider_pubkey_hex: *const c_char,
    provider_display_name: *const c_char,
    relay_urls: *mut *mut c_char,
    url_count: usize,
    out_count: *mut usize,
    error: *mut *mut FfiError,
) -> *mut FfiDiscoveredTool {
    let guard = kv::get::<contextvm_sdk::RelayPool>(pool_handle);
    let pool = match guard {
        Some(p) => p,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid pool handle".into(),
                },
            );
            return ptr::null_mut();
        }
    };
    let provider_pubkey = match c_str_to_string(provider_pubkey_hex) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: "null provider pubkey".into(),
                },
            );
            return ptr::null_mut();
        }
    };
    let provider_display_name = c_str_to_string(provider_display_name);
    let urls = c_str_array_to_vec(relay_urls, url_count);
    let client = pool.client();

    let result = global_runtime().block_on(async {
        crate::discovery::discover_tools(client, &provider_pubkey, provider_display_name, &urls)
            .await
    });

    match result {
        Ok(tools) => tools_to_ffi_array(tools, out_count),
        Err(e) => {
            set_error(error, e.into());
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_discover_all_tools(
    pool_handle: FfiHandle,
    relay_urls: *mut *mut c_char,
    url_count: usize,
    out_count: *mut usize,
    error: *mut *mut FfiError,
) -> *mut FfiDiscoveredTool {
    let guard = kv::get::<contextvm_sdk::RelayPool>(pool_handle);
    let pool = match guard {
        Some(p) => p,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid pool handle".into(),
                },
            );
            return ptr::null_mut();
        }
    };
    let urls = c_str_array_to_vec(relay_urls, url_count);
    let client = pool.client();

    let result = global_runtime()
        .block_on(async { crate::discovery::discover_all_tools(client, &urls).await });

    match result {
        Ok(tools) => tools_to_ffi_array(tools, out_count),
        Err(e) => {
            set_error(error, e.into());
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_fetch_provider_profiles(
    pool_handle: FfiHandle,
    provider_pubkeys: *mut *mut c_char,
    provider_pubkey_count: usize,
    relay_urls: *mut *mut c_char,
    url_count: usize,
    out_count: *mut usize,
    error: *mut *mut FfiError,
) -> *mut FfiProviderProfile {
    let guard = kv::get::<contextvm_sdk::RelayPool>(pool_handle);
    let pool = match guard {
        Some(p) => p,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid pool handle".into(),
                },
            );
            return ptr::null_mut();
        }
    };
    let pubkeys = c_str_array_to_vec(provider_pubkeys, provider_pubkey_count);
    let urls = c_str_array_to_vec(relay_urls, url_count);
    let client = pool.client();

    let result = global_runtime().block_on(async {
        crate::discovery::fetch_provider_profiles(client, &pubkeys, &urls).await
    });

    match result {
        Ok(profiles) => profiles_to_ffi_array(profiles.into_values().collect(), out_count),
        Err(e) => {
            set_error(error, e.into());
            ptr::null_mut()
        }
    }
}

// ─── Encryption ────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cvm_encrypt_nip44(
    keys_handle: FfiHandle,
    recipient_pubkey_hex: *const c_char,
    plaintext: *const c_char,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let keys = match kv::get::<contextvm_sdk::signer::Keys>(keys_handle) {
        Some(k) => k.as_ref().clone(),
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid key handle".into(),
                },
            );
            return ptr::null_mut();
        }
    };

    let pk_str = match c_str_to_string(recipient_pubkey_hex) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: "null recipient pubkey".into(),
                },
            );
            return ptr::null_mut();
        }
    };
    let pk = match contextvm_sdk::signer::PublicKey::from_hex(&pk_str) {
        Ok(pk) => pk,
        Err(e) => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: format!("invalid pubkey: {e}"),
                },
            );
            return ptr::null_mut();
        }
    };
    let pt = match c_str_to_string(plaintext) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: "null plaintext".into(),
                },
            );
            return ptr::null_mut();
        }
    };

    match global_runtime().block_on(contextvm_sdk::encryption::encrypt_nip44(&keys, &pk, &pt)) {
        Ok(ct) => string_to_c(ct),
        Err(e) => {
            set_error(error, e.into());
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_decrypt_nip44(
    keys_handle: FfiHandle,
    sender_pubkey_hex: *const c_char,
    ciphertext: *const c_char,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let keys = match kv::get::<contextvm_sdk::signer::Keys>(keys_handle) {
        Some(k) => k.as_ref().clone(),
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Other,
                    message: "invalid key handle".into(),
                },
            );
            return ptr::null_mut();
        }
    };

    let pk_str = match c_str_to_string(sender_pubkey_hex) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: "null sender pubkey".into(),
                },
            );
            return ptr::null_mut();
        }
    };
    let pk = match contextvm_sdk::signer::PublicKey::from_hex(&pk_str) {
        Ok(pk) => pk,
        Err(e) => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: format!("invalid pubkey: {e}"),
                },
            );
            return ptr::null_mut();
        }
    };
    let ct = match c_str_to_string(ciphertext) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: "null ciphertext".into(),
                },
            );
            return ptr::null_mut();
        }
    };

    match global_runtime().block_on(contextvm_sdk::encryption::decrypt_nip44(&keys, &pk, &ct)) {
        Ok(pt) => string_to_c(pt),
        Err(e) => {
            set_error(error, e.into());
            ptr::null_mut()
        }
    }
}

// ─── Utility ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn cvm_version() -> *mut c_char {
    string_to_c(env!("CARGO_PKG_VERSION").to_string())
}

#[no_mangle]
pub extern "C" fn cvm_pubkey_hex_to_npub(
    pubkey_hex: *const c_char,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let pubkey = match c_str_to_string(pubkey_hex) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Validation,
                    message: "null pubkey".into(),
                },
            );
            return ptr::null_mut();
        }
    };

    match crate::discovery::pubkey_hex_to_npub(&pubkey) {
        Ok(npub) => string_to_c(npub),
        Err(e) => {
            set_error(error, e.into());
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_error_free(e: *mut FfiError) {
    if !e.is_null() {
        unsafe {
            let _ = Box::from_raw(e);
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_error_message(e: *const FfiError) -> *mut c_char {
    if e.is_null() {
        return ptr::null_mut();
    }
    unsafe { string_to_c((*e).message.clone()) }
}

#[no_mangle]
pub extern "C" fn cvm_error_code(e: *const FfiError) -> ErrorCode {
    if e.is_null() {
        return ErrorCode::Ok;
    }
    unsafe { (*e).code }
}

fn tools_to_ffi_array(
    tools: Vec<crate::discovery::DiscoveredToolRecord>,
    out_count: *mut usize,
) -> *mut FfiDiscoveredTool {
    let count = tools.len();
    let ffi_tools: Vec<FfiDiscoveredTool> = tools
        .into_iter()
        .map(|tool| FfiDiscoveredTool {
            provider_pubkey: string_to_c(tool.provider_pubkey),
            provider_display_name: opt_string_to_c(tool.provider_display_name),
            provider_name: opt_string_to_c(tool.provider_name),
            provider_about: opt_string_to_c(tool.provider_about),
            provider_picture: opt_string_to_c(tool.provider_picture),
            provider_nip05: opt_string_to_c(tool.provider_nip05),
            tool_name: string_to_c(tool.tool_name),
            description: string_to_c(tool.description),
            schema_json: string_to_c(tool.schema_json),
        })
        .collect();

    unsafe {
        if !out_count.is_null() {
            *out_count = count;
        }
    }

    let mut ffi_tools = ffi_tools;
    let ptr = ffi_tools.as_mut_ptr();
    std::mem::forget(ffi_tools);
    ptr
}

fn profiles_to_ffi_array(
    profiles: Vec<crate::discovery::ProviderProfileRecord>,
    out_count: *mut usize,
) -> *mut FfiProviderProfile {
    let count = profiles.len();
    let ffi_profiles: Vec<FfiProviderProfile> = profiles
        .into_iter()
        .map(|profile| FfiProviderProfile {
            pubkey: string_to_c(profile.pubkey),
            name: opt_string_to_c(profile.name),
            about: opt_string_to_c(profile.about),
            picture: opt_string_to_c(profile.picture),
            nip05: opt_string_to_c(profile.nip05),
        })
        .collect();

    unsafe {
        if !out_count.is_null() {
            *out_count = count;
        }
    }

    let mut ffi_profiles = ffi_profiles;
    let ptr = ffi_profiles.as_mut_ptr();
    std::mem::forget(ffi_profiles);
    ptr
}
