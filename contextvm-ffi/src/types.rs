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

/// Raw integer mode value supplied by C callers.
pub type FfiMode = i32;

pub const ENCRYPTION_MODE_OPTIONAL: FfiMode = 0;
pub const ENCRYPTION_MODE_REQUIRED: FfiMode = 1;
pub const ENCRYPTION_MODE_DISABLED: FfiMode = 2;

pub const GIFT_WRAP_MODE_OPTIONAL: FfiMode = 0;
pub const GIFT_WRAP_MODE_EPHEMERAL: FfiMode = 1;
pub const GIFT_WRAP_MODE_PERSISTENT: FfiMode = 2;

/// JSON-RPC message type discriminator.
pub type JsonRpcType = i32;

pub const JSON_RPC_TYPE_REQUEST: JsonRpcType = 0;
pub const JSON_RPC_TYPE_RESPONSE: JsonRpcType = 1;
pub const JSON_RPC_TYPE_ERROR_RESPONSE: JsonRpcType = 2;
pub const JSON_RPC_TYPE_NOTIFICATION: JsonRpcType = 3;

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

/// A capability exclusion pattern that bypasses pubkey whitelisting.
#[repr(C)]
#[derive(Debug)]
pub struct FfiCapabilityExclusion {
    pub method: *mut c_char,
    pub name: *mut c_char,
}

/// Learned peer capability flags.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfiPeerCapabilities {
    pub supports_encryption: bool,
    pub supports_ephemeral_encryption: bool,
    pub supports_oversized_transfer: bool,
}

/// Server transport config for FFI.
#[repr(C)]
#[derive(Debug)]
pub struct FfiServerConfig {
    pub relay_urls: *mut *mut c_char,
    pub relay_url_count: usize,
    pub encryption_mode: FfiMode,
    pub gift_wrap_mode: FfiMode,
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
    pub excluded_capabilities: *mut FfiCapabilityExclusion,
    pub excluded_capability_count: usize,
    pub max_sessions: usize,
    pub request_timeout_secs: u64,
    pub relay_list_urls: *mut *mut c_char,
    pub relay_list_url_count: usize,
    pub bootstrap_relay_urls: *mut *mut c_char,
    pub bootstrap_relay_url_count: usize,
    pub publish_relay_list: bool,
    pub profile_metadata_json: *mut c_char,
}

/// Client transport config for FFI.
#[repr(C)]
#[derive(Debug)]
pub struct FfiClientConfig {
    pub relay_urls: *mut *mut c_char,
    pub relay_url_count: usize,
    pub server_pubkey: *mut c_char,
    pub encryption_mode: FfiMode,
    pub gift_wrap_mode: FfiMode,
    pub is_stateless: bool,
    pub timeout_secs: u64,
    pub discovery_relay_urls: *mut *mut c_char,
    pub discovery_relay_url_count: usize,
    pub fallback_operational_relay_urls: *mut *mut c_char,
    pub fallback_operational_relay_url_count: usize,
}

// ─── Internal conversion helpers ───────────────────────────────────────

pub fn c_str_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr).to_str().ok().map(String::from) }
}

pub fn c_str_to_string_checked(ptr: *const c_char, name: &str) -> Result<String, FfiError> {
    if ptr.is_null() {
        return Err(FfiError {
            code: ErrorCode::Validation,
            message: format!("null {name}"),
        });
    }

    unsafe {
        CStr::from_ptr(ptr)
            .to_str()
            .map(String::from)
            .map_err(|e| FfiError {
                code: ErrorCode::Validation,
                message: format!("{name} is not valid UTF-8: {e}"),
            })
    }
}

pub fn optional_c_str_to_string_checked(
    ptr: *const c_char,
    name: &str,
) -> Result<Option<String>, FfiError> {
    if ptr.is_null() {
        Ok(None)
    } else {
        c_str_to_string_checked(ptr, name).map(Some)
    }
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

pub fn c_str_array_to_vec_checked(
    ptr: *mut *mut c_char,
    count: usize,
    name: &str,
) -> Result<Vec<String>, FfiError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(FfiError {
            code: ErrorCode::Validation,
            message: format!("{name} has count {count} but null pointer"),
        });
    }

    unsafe {
        std::slice::from_raw_parts(ptr, count)
            .iter()
            .enumerate()
            .map(|(index, &p)| {
                if p.is_null() {
                    return Err(FfiError {
                        code: ErrorCode::Validation,
                        message: format!("{name}[{index}] is null"),
                    });
                }

                CStr::from_ptr(p)
                    .to_str()
                    .map(String::from)
                    .map_err(|e| FfiError {
                        code: ErrorCode::Validation,
                        message: format!("{name}[{index}] is not valid UTF-8: {e}"),
                    })
            })
            .collect()
    }
}

pub fn ffi_encryption_mode_to_sdk(
    mode: FfiMode,
) -> Result<contextvm_sdk::EncryptionMode, FfiError> {
    match mode {
        ENCRYPTION_MODE_OPTIONAL => Ok(contextvm_sdk::EncryptionMode::Optional),
        ENCRYPTION_MODE_REQUIRED => Ok(contextvm_sdk::EncryptionMode::Required),
        ENCRYPTION_MODE_DISABLED => Ok(contextvm_sdk::EncryptionMode::Disabled),
        _ => Err(FfiError {
            code: ErrorCode::Validation,
            message: format!("invalid encryption_mode {mode}"),
        }),
    }
}

pub fn ffi_gift_wrap_mode_to_sdk(mode: FfiMode) -> Result<contextvm_sdk::GiftWrapMode, FfiError> {
    match mode {
        GIFT_WRAP_MODE_OPTIONAL => Ok(contextvm_sdk::GiftWrapMode::Optional),
        GIFT_WRAP_MODE_EPHEMERAL => Ok(contextvm_sdk::GiftWrapMode::Ephemeral),
        GIFT_WRAP_MODE_PERSISTENT => Ok(contextvm_sdk::GiftWrapMode::Persistent),
        _ => Err(FfiError {
            code: ErrorCode::Validation,
            message: format!("invalid gift_wrap_mode {mode}"),
        }),
    }
}

pub fn json_rpc_id_to_string(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn peer_capabilities_to_ffi(
    caps: contextvm_sdk::transport::discovery_tags::PeerCapabilities,
) -> FfiPeerCapabilities {
    FfiPeerCapabilities {
        supports_encryption: caps.supports_encryption,
        supports_ephemeral_encryption: caps.supports_ephemeral_encryption,
        supports_oversized_transfer: caps.supports_oversized_transfer,
    }
}

/// Convert an SDK `JsonRpcMessage` to the FFI representation.
pub fn message_to_ffi(msg: &contextvm_sdk::JsonRpcMessage) -> FfiJsonRpcMessage {
    let json_str = serde_json::to_string(msg).unwrap_or_default();
    let msg_type = match msg {
        contextvm_sdk::JsonRpcMessage::Request(_) => JSON_RPC_TYPE_REQUEST,
        contextvm_sdk::JsonRpcMessage::Response(_) => JSON_RPC_TYPE_RESPONSE,
        contextvm_sdk::JsonRpcMessage::ErrorResponse(_) => JSON_RPC_TYPE_ERROR_RESPONSE,
        contextvm_sdk::JsonRpcMessage::Notification(_) => JSON_RPC_TYPE_NOTIFICATION,
    };
    FfiJsonRpcMessage {
        msg_type,
        payload_json: string_to_c(json_str),
        method: opt_string_to_c(msg.method().map(String::from)),
        id: opt_string_to_c(msg.id().map(json_rpc_id_to_string)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_to_ffi_string_id_is_unquoted() {
        let msg = contextvm_sdk::JsonRpcMessage::Request(contextvm_sdk::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::Value::String("request-1".to_string()),
            method: "tools/list".to_string(),
            params: None,
        });

        let ffi = message_to_ffi(&msg);
        let id = unsafe { CStr::from_ptr(ffi.id).to_str().unwrap().to_string() };
        cvm_message_free(ffi);

        assert_eq!(id, "request-1");
    }

    #[test]
    fn message_to_ffi_non_string_id_remains_json_encoded() {
        let msg = contextvm_sdk::JsonRpcMessage::Response(contextvm_sdk::JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(42),
            result: serde_json::json!({}),
        });

        let ffi = message_to_ffi(&msg);
        let id = unsafe { CStr::from_ptr(ffi.id).to_str().unwrap().to_string() };
        cvm_message_free(ffi);

        assert_eq!(id, "42");
    }

    // Error handling lifecycle tests
    mod error_lifecycle_tests {
        use super::*;
        use crate::error::{set_error, ErrorCode, FfiError};

        #[test]
        fn test_error_creation_and_free() {
            let mut error_ptr: *mut FfiError = std::ptr::null_mut();

            // Create an error using set_error
            let test_error = FfiError {
                code: ErrorCode::Transport,
                message: "test error message".to_string(),
            };
            set_error(&mut error_ptr, test_error);

            // Verify error was created with correct code
            assert!(!error_ptr.is_null());
            unsafe {
                assert_eq!((*error_ptr).code, ErrorCode::Transport);
                // Verify the message round-tripped intact
                assert_eq!((*error_ptr).message, "test error message");
            }

            // Free the error - this should not panic
            cvm_error_free(error_ptr);
        }

        #[test]
        fn test_error_free_null_is_safe() {
            // Should not panic on null
            cvm_error_free(std::ptr::null_mut());
        }

        #[test]
        fn test_error_code_values() {
            // Verify error code discriminant values match C header
            assert_eq!(ErrorCode::Ok as i32, 0);
            assert_eq!(ErrorCode::Transport as i32, 1);
            assert_eq!(ErrorCode::Encryption as i32, 2);
            assert_eq!(ErrorCode::Decryption as i32, 3);
            assert_eq!(ErrorCode::Timeout as i32, 4);
            assert_eq!(ErrorCode::Validation as i32, 5);
            assert_eq!(ErrorCode::Unauthorized as i32, 6);
            assert_eq!(ErrorCode::Serialization as i32, 7);
            assert_eq!(ErrorCode::Other as i32, 99);
        }

        #[test]
        fn test_error_display_format() {
            let error = FfiError {
                code: ErrorCode::Timeout,
                message: "operation timed out".to_string(),
            };
            let display = format!("{}", error);
            assert!(display.contains("Timeout"));
            assert!(display.contains("operation timed out"));
        }

        #[test]
        fn test_error_clone() {
            let original = FfiError {
                code: ErrorCode::Validation,
                message: "validation failed".to_string(),
            };
            let cloned = original.clone();

            assert_eq!(original.code, cloned.code);
            assert_eq!(original.message, cloned.message);
        }

        #[test]
        fn test_incoming_request_free_null_is_safe() {
            cvm_incoming_request_free(FfiIncomingRequest {
                message: FfiJsonRpcMessage {
                    msg_type: JSON_RPC_TYPE_REQUEST,
                    id: std::ptr::null_mut(),
                    payload_json: std::ptr::null_mut(),
                    method: std::ptr::null_mut(),
                },
                client_pubkey: std::ptr::null_mut(),
                event_id: std::ptr::null_mut(),
                is_encrypted: false,
            });
        }

        #[test]
        fn test_string_free_null_is_safe() {
            cvm_string_free(std::ptr::null_mut());
        }

        #[test]
        fn test_message_free_null_is_safe() {
            cvm_message_free(FfiJsonRpcMessage {
                msg_type: JSON_RPC_TYPE_RESPONSE,
                id: std::ptr::null_mut(),
                payload_json: std::ptr::null_mut(),
                method: std::ptr::null_mut(),
            });
        }
    }
}
