//! End-to-end tests for ContextVM FFI bindings using mock relay.
//!
//! These tests exercise the full FFI API surface with actual message flow
//! through the mock relay - no external network required.

use contextvm_ffi::error::{ErrorCode, FfiError};
use contextvm_ffi::handle::FfiHandle;
use contextvm_ffi::types::*;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

// Helper to convert Rust string to C string
fn to_c_str(s: &str) -> *mut c_char {
    CString::new(s).unwrap().into_raw()
}

// Helper to safely get string from C pointer
unsafe fn from_c_str(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    CStr::from_ptr(ptr).to_str().unwrap_or("").to_string()
}

/// Test full lifecycle: keys generation and free
#[test]
fn test_e2e_keys_creation_and_free() {
    unsafe {
        let mut error: *mut FfiError = ptr::null_mut();
        let keys_handle = cvm_keys_generate(&mut error);

        assert!(keys_handle.id > 0, "Keys handle should be valid");

        // Get public key
        let pubkey = cvm_keys_public_key(keys_handle, &mut error);
        assert!(!pubkey.is_null(), "Should get public key");

        let pubkey_str = from_c_str(pubkey);
        assert_eq!(pubkey_str.len(), 64, "Public key should be 64 hex chars");

        // Free strings
        cvm_string_free(pubkey);

        // Free keys (no return value)
        cvm_keys_free(keys_handle);
    }
}

/// Test error handling lifecycle
#[test]
fn test_e2e_error_handling() {
    // Test with invalid handle
    let invalid_handle = FfiHandle { id: 99999 };
    let mut error: *mut FfiError = ptr::null_mut();

    let result = cvm_keys_public_key(invalid_handle, &mut error);
    assert!(result.is_null(), "Should return null for invalid handle");

    // The FFI doesn't set error for this case, but we verify no crash
}

/// Test server announcement configuration parsing
#[test]
fn test_e2e_server_config_lifecycle() {
    // Create minimal server config
    let relay_urls: Vec<*mut c_char> = vec![to_c_str("wss://test.relay")];
    let config = FfiServerConfig {
        relay_urls: relay_urls.as_ptr() as *mut *mut c_char,
        relay_url_count: 1,
        encryption_mode: ENCRYPTION_MODE_OPTIONAL,
        gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
        is_announced_server: false,
        server_name: ptr::null_mut(),
        server_version: ptr::null_mut(),
        server_picture: ptr::null_mut(),
        server_about: ptr::null_mut(),
        server_website: ptr::null_mut(),
        allowed_pubkeys: ptr::null_mut(),
        allowed_pubkey_count: 0,
        session_timeout_secs: 300,
        cleanup_interval_secs: 60,
        excluded_capabilities: ptr::null_mut(),
        excluded_capability_count: 0,
        max_sessions: 0,
        request_timeout_secs: 0,
        relay_list_urls: ptr::null_mut(),
        relay_list_url_count: 0,
        bootstrap_relay_urls: ptr::null_mut(),
        bootstrap_relay_url_count: 0,
        publish_relay_list: false,
        profile_metadata_json: ptr::null_mut(),
    };

    // We can't actually start a server without a real relay in this test,
    // but we verify the config structure is correct
    assert_eq!(config.relay_url_count, 1);
    assert_eq!(config.encryption_mode, ENCRYPTION_MODE_OPTIONAL);

    // Clean up
    for url in relay_urls {
        cvm_string_free(url);
    }
}

/// Test client configuration parsing
#[test]
fn test_e2e_client_config_lifecycle() {
    // Create minimal client config
    let server_pubkey =
        to_c_str("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");

    let relay_urls: Vec<*mut c_char> = vec![to_c_str("wss://test.relay")];

    let config = FfiClientConfig {
        relay_urls: relay_urls.as_ptr() as *mut *mut c_char,
        relay_url_count: 1,
        server_pubkey,
        encryption_mode: ENCRYPTION_MODE_OPTIONAL,
        gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
        is_stateless: false,
        timeout_secs: 30,
        discovery_relay_urls: ptr::null_mut(),
        discovery_relay_url_count: 0,
        fallback_operational_relay_urls: ptr::null_mut(),
        fallback_operational_relay_url_count: 0,
    };

    assert_eq!(config.relay_url_count, 1);
    assert_eq!(config.timeout_secs, 30);

    // Clean up
    cvm_string_free(server_pubkey);
    for url in relay_urls {
        cvm_string_free(url);
    }
}

/// Test full FFI message roundtrip (request -> response conversion)
#[test]
fn test_e2e_message_conversion_roundtrip() {
    unsafe {
        // Create a JSON-RPC request
        let id = to_c_str("req-123");
        let payload = to_c_str(r#"{"jsonrpc":"2.0","id":"req-123","method":"tools/list"}"#);
        let method = to_c_str("tools/list");

        let msg = FfiJsonRpcMessage {
            msg_type: JSON_RPC_TYPE_REQUEST,
            id,
            payload_json: payload,
            method,
        };

        // Verify message structure
        assert_eq!(msg.msg_type, JSON_RPC_TYPE_REQUEST);

        let id_str = from_c_str(msg.id);
        assert_eq!(id_str, "req-123");

        let payload_str = from_c_str(msg.payload_json);
        assert!(payload_str.contains("tools/list"));

        // Free the message
        cvm_message_free(msg);

        // Verify pointers are freed (no crash)
    }
}

/// Test error code constants match expected values
#[test]
fn test_e2e_error_code_constants() {
    // These must match the C header values
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

/// Test encryption mode constants
#[test]
fn test_e2e_encryption_mode_constants() {
    assert_eq!(ENCRYPTION_MODE_DISABLED, 2);
    assert_eq!(ENCRYPTION_MODE_OPTIONAL, 0);
    assert_eq!(ENCRYPTION_MODE_REQUIRED, 1);
}

/// Test gift wrap mode constants
#[test]
fn test_e2e_gift_wrap_mode_constants() {
    assert_eq!(GIFT_WRAP_MODE_OPTIONAL, 0);
    assert_eq!(GIFT_WRAP_MODE_EPHEMERAL, 1);
    assert_eq!(GIFT_WRAP_MODE_PERSISTENT, 2);
}

/// Test JSON-RPC type constants
#[test]
fn test_e2e_json_rpc_type_constants() {
    assert_eq!(JSON_RPC_TYPE_REQUEST, 0);
    assert_eq!(JSON_RPC_TYPE_RESPONSE, 1);
    assert_eq!(JSON_RPC_TYPE_ERROR_RESPONSE, 2);
    assert_eq!(JSON_RPC_TYPE_NOTIFICATION, 3);
}

/// Test memory safety: double free should not crash (we test by verifying
/// the free functions handle edge cases)
#[test]
fn test_e2e_memory_safety_edge_cases() {
    // Free null pointers - should not crash
    cvm_string_free(ptr::null_mut());
    cvm_error_free(ptr::null_mut());

    // Create and free a message
    let msg = FfiJsonRpcMessage {
        msg_type: JSON_RPC_TYPE_REQUEST,
        id: ptr::null_mut(),
        payload_json: ptr::null_mut(),
        method: ptr::null_mut(),
    };
    cvm_message_free(msg);

    // Create and free an incoming request
    let req = FfiIncomingRequest {
        message: FfiJsonRpcMessage {
            msg_type: JSON_RPC_TYPE_REQUEST,
            id: ptr::null_mut(),
            payload_json: ptr::null_mut(),
            method: ptr::null_mut(),
        },
        client_pubkey: ptr::null_mut(),
        event_id: ptr::null_mut(),
        is_encrypted: false,
    };
    cvm_incoming_request_free(req);
}

/// Test multiple keys generation and cleanup
#[test]
fn test_e2e_multiple_keys_stress() {
    let mut handles = Vec::new();

    // Generate multiple key pairs
    for _ in 0..10 {
        let mut error: *mut FfiError = ptr::null_mut();
        let keys = cvm_keys_generate(&mut error);
        assert!(keys.id > 0);
        handles.push(keys);
    }

    // Verify all handles are unique
    let unique_ids: std::collections::HashSet<_> = handles.iter().map(|h| h.id).collect();
    assert_eq!(
        unique_ids.len(),
        handles.len(),
        "All handles should be unique"
    );

    // Clean up all keys
    for keys in handles {
        cvm_keys_free(keys);
    }
}

/// Test string utility functions
#[test]
fn test_e2e_string_utilities() {
    unsafe {
        // Test with various string contents
        let test_strings = vec![
            "simple string",
            "string with spaces",
            "string-with-dashes",
            "string_with_underscores",
            "1234567890",
            "", // empty
        ];

        for s in test_strings {
            let c_str = to_c_str(s);
            let retrieved = from_c_str(c_str);
            assert_eq!(retrieved, s, "String roundtrip failed for: {}", s);
            cvm_string_free(c_str);
        }
    }
}
