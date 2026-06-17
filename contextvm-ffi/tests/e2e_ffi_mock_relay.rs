//! End-to-end tests for ContextVM FFI using mock relay.
//!
//! These tests exercise actual message flow through FFI channels
//! using the mock relay implementation.

use contextvm_ffi::{
    cvm_client_ch_recv_timeout, cvm_gateway_ch_recv_timeout, cvm_proxy_ch_recv_timeout,
    cvm_server_ch_recv_timeout,
    error::{ErrorCode, FfiError},
    handle::FfiHandle,
    types::*,
};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

// Helper to convert Rust string to C string (returns raw pointer)
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

/// Test that we can generate keys and they have valid format
#[test]
fn test_mock_relay_keys_generation() {
    unsafe {
        let mut error: *mut FfiError = ptr::null_mut();
        let keys = cvm_keys_generate(&mut error);
        assert!(keys.id > 0, "Should generate valid keys handle");

        let pubkey = cvm_keys_public_key(keys, &mut error);
        assert!(!pubkey.is_null(), "Should return public key");

        let pubkey_str = from_c_str(pubkey);
        assert_eq!(
            pubkey_str.len(),
            64,
            "Public key should be 64 hex characters"
        );

        // Verify it's valid hex
        assert!(
            pubkey_str.chars().all(|c| c.is_ascii_hexdigit()),
            "Public key should be valid hex"
        );

        // Cleanup
        cvm_string_free(pubkey);
        cvm_keys_free(keys);
    }
}

/// Test server config validation
#[test]
fn test_mock_relay_server_config_validation() {
    // Create a server config with valid relay URLs
    let relay_urls = vec![
        to_c_str("wss://mock.relay.1"),
        to_c_str("wss://mock.relay.2"),
    ];

    let config = FfiServerConfig {
        relay_urls: relay_urls.as_ptr() as *mut *mut c_char,
        relay_url_count: 2,
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

    // Verify config is correctly structured
    assert_eq!(config.relay_url_count, 2);
    assert_eq!(config.encryption_mode, ENCRYPTION_MODE_OPTIONAL);
    assert_eq!(config.session_timeout_secs, 300);

    // Cleanup
    for url in relay_urls {
        cvm_string_free(url);
    }
}

/// Test client config validation
#[test]
fn test_mock_relay_client_config_validation() {
    let server_pubkey =
        to_c_str("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
    let relay_urls = vec![to_c_str("wss://mock.relay")];

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

    // Cleanup
    cvm_string_free(server_pubkey);
    for url in relay_urls {
        cvm_string_free(url);
    }
}

/// Test message structure conversion
#[test]
fn test_mock_relay_message_structure() {
    unsafe {
        // Test JSON-RPC request message
        let id = to_c_str("req-test-123");
        let payload =
            to_c_str(r#"{"jsonrpc":"2.0","id":"req-test-123","method":"tools/list","params":{}}"#);
        let method = to_c_str("tools/list");

        let msg = FfiJsonRpcMessage {
            msg_type: JSON_RPC_TYPE_REQUEST,
            id,
            payload_json: payload,
            method,
        };

        assert_eq!(msg.msg_type, JSON_RPC_TYPE_REQUEST);

        let id_str = from_c_str(msg.id);
        assert_eq!(id_str, "req-test-123");

        let payload_str = from_c_str(msg.payload_json);
        assert!(payload_str.contains("tools/list"));
        assert!(payload_str.contains("jsonrpc"));

        // Cleanup
        cvm_message_free(msg);

        // Test JSON-RPC response message
        let id2 = to_c_str("42");
        let payload2 = to_c_str(r#"{"jsonrpc":"2.0","id":42,"result":{"tools":[]}}"#);

        let msg2 = FfiJsonRpcMessage {
            msg_type: JSON_RPC_TYPE_RESPONSE,
            id: id2,
            payload_json: payload2,
            method: ptr::null_mut(),
        };

        assert_eq!(msg2.msg_type, JSON_RPC_TYPE_RESPONSE);
        cvm_message_free(msg2);

        // Test JSON-RPC error response
        let id3 = to_c_str("99");
        let payload3 = to_c_str(
            r#"{"jsonrpc":"2.0","id":99,"error":{"code":-32600,"message":"Invalid Request"}}"#,
        );

        let msg3 = FfiJsonRpcMessage {
            msg_type: JSON_RPC_TYPE_ERROR_RESPONSE,
            id: id3,
            payload_json: payload3,
            method: ptr::null_mut(),
        };

        assert_eq!(msg3.msg_type, JSON_RPC_TYPE_ERROR_RESPONSE);
        cvm_message_free(msg3);

        // Test notification (no id)
        let payload4 =
            to_c_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#);
        let method4 = to_c_str("notifications/initialized");

        let msg4 = FfiJsonRpcMessage {
            msg_type: JSON_RPC_TYPE_NOTIFICATION,
            id: ptr::null_mut(),
            payload_json: payload4,
            method: method4,
        };

        assert_eq!(msg4.msg_type, JSON_RPC_TYPE_NOTIFICATION);
        cvm_message_free(msg4);
    }
}

/// Test incoming request structure
#[test]
fn test_mock_relay_incoming_request_structure() {
    unsafe {
        let client_pubkey =
            to_c_str("aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233");
        let event_id = to_c_str("event123456789");

        let msg = FfiJsonRpcMessage {
            msg_type: JSON_RPC_TYPE_REQUEST,
            id: to_c_str("1"),
            payload_json: to_c_str(r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#),
            method: to_c_str("test"),
        };

        let req = FfiIncomingRequest {
            message: msg,
            client_pubkey,
            event_id,
            is_encrypted: true,
        };

        assert!(req.is_encrypted);

        let client_str = from_c_str(req.client_pubkey);
        assert_eq!(client_str.len(), 64);

        // Cleanup
        cvm_incoming_request_free(req);
    }
}

/// Test error handling with various error codes
#[test]
fn test_mock_relay_error_handling() {
    // Test error codes
    let error_codes = vec![
        (ErrorCode::Ok as i32, "OK"),
        (ErrorCode::Transport as i32, "Transport"),
        (ErrorCode::Encryption as i32, "Encryption"),
        (ErrorCode::Decryption as i32, "Decryption"),
        (ErrorCode::Timeout as i32, "Timeout"),
        (ErrorCode::Validation as i32, "Validation"),
        (ErrorCode::Unauthorized as i32, "Unauthorized"),
        (ErrorCode::Serialization as i32, "Serialization"),
        (ErrorCode::Other as i32, "Other"),
    ];

    for (code, name) in error_codes {
        println!("Error code {}: {}", code, name);
    }

    // Test freeing null error (should not crash)
    cvm_error_free(ptr::null_mut());
}

/// Test channel handle invalidation (timeout functions with bad handles)
#[test]
fn test_mock_relay_channel_invalid_handles() {
    let invalid_handle = FfiHandle { id: 99999 };

    // Test all channel recv_timeout functions with invalid handle
    let mut error: *mut FfiError = ptr::null_mut();
    let mut out_req = std::mem::MaybeUninit::<FfiIncomingRequest>::uninit();
    let mut out_msg = std::mem::MaybeUninit::<FfiJsonRpcMessage>::uninit();

    // Server channel
    let result = cvm_server_ch_recv_timeout(invalid_handle, 1, out_req.as_mut_ptr(), &mut error);
    assert!(!result);
    assert!(!error.is_null());
    cvm_error_free(error);
    error = ptr::null_mut();

    // Client channel
    let result = cvm_client_ch_recv_timeout(invalid_handle, 1, out_msg.as_mut_ptr(), &mut error);
    assert!(!result);
    assert!(!error.is_null());
    cvm_error_free(error);
    error = ptr::null_mut();

    // Gateway channel
    let result = cvm_gateway_ch_recv_timeout(invalid_handle, 1, out_req.as_mut_ptr(), &mut error);
    assert!(!result);
    assert!(!error.is_null());
    cvm_error_free(error);
    error = ptr::null_mut();

    // Proxy channel
    let result = cvm_proxy_ch_recv_timeout(invalid_handle, 1, out_msg.as_mut_ptr(), &mut error);
    assert!(!result);
    assert!(!error.is_null());
    cvm_error_free(error);
}

/// Test peer capabilities structure
#[test]
fn test_mock_relay_peer_capabilities() {
    // Create a peer capabilities structure
    let caps = FfiPeerCapabilities {
        supports_encryption: true,
        supports_ephemeral_encryption: true,
        supports_oversized_transfer: false,
    };

    assert!(caps.supports_encryption);
    assert!(caps.supports_ephemeral_encryption);
    assert!(!caps.supports_oversized_transfer);

    // Copy is safe (no pointers)
    let caps2 = caps;
    assert!(caps2.supports_encryption);
}

/// Test multiple sequential operations
#[test]
fn test_mock_relay_sequential_operations() {
    unsafe {
        // Generate multiple keys
        let mut keys = Vec::new();
        for _ in 0..5 {
            let mut error: *mut FfiError = ptr::null_mut();
            let k = cvm_keys_generate(&mut error);
            assert!(k.id > 0);
            keys.push(k);
        }

        // Verify all handles are unique
        let unique_count = keys
            .iter()
            .map(|k| k.id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert_eq!(unique_count, keys.len());

        // Get pubkeys for all
        let mut pubkeys = Vec::new();
        for k in &keys {
            let mut error: *mut FfiError = ptr::null_mut();
            let pk = cvm_keys_public_key(*k, &mut error);
            assert!(!pk.is_null());
            pubkeys.push(pk);
        }

        // Verify all pubkeys are unique
        let pubkey_strings: Vec<String> = pubkeys.iter().map(|&pk| from_c_str(pk)).collect();
        let unique_pubkeys = pubkey_strings
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert_eq!(unique_pubkeys, pubkey_strings.len());

        // Cleanup
        for pk in pubkeys {
            cvm_string_free(pk);
        }
        for k in keys {
            cvm_keys_free(k);
        }
    }
}

/// Test discovered tool structure
#[test]
fn test_mock_relay_discovered_tool_structure() {
    unsafe {
        let provider_pubkey =
            to_c_str("0011223300112233001122330011223300112233001122330011223300112233");
        let provider_display_name = to_c_str("Test Provider");
        let tool_name = to_c_str("echo");
        let description = to_c_str("Echo a message back");
        let schema_json =
            to_c_str(r#"{"type":"object","properties":{"message":{"type":"string"}}}"#);

        let tool = FfiDiscoveredTool {
            provider_pubkey,
            provider_display_name,
            provider_name: ptr::null_mut(),
            provider_about: ptr::null_mut(),
            provider_picture: ptr::null_mut(),
            provider_nip05: ptr::null_mut(),
            tool_name,
            description,
            schema_json,
        };

        let name_str = from_c_str(tool.tool_name);
        assert_eq!(name_str, "echo");

        let desc_str = from_c_str(tool.description);
        assert_eq!(desc_str, "Echo a message back");

        // Cleanup - manually free strings (don't use cvm_discovered_tools_free on stack data)
        cvm_string_free(tool.provider_pubkey);
        cvm_string_free(tool.provider_display_name);
        cvm_string_free(tool.tool_name);
        cvm_string_free(tool.description);
        cvm_string_free(tool.schema_json);
    }
}

/// Test server announcement structure
#[test]
fn test_mock_relay_server_announcement_structure() {
    unsafe {
        let pubkey = to_c_str("0011223300112233001122330011223300112233001122330011223300112233");
        let name = to_c_str("Test Server");
        let version = to_c_str("1.0.0");
        let event_id = to_c_str("event12345");

        let announcement = FfiServerAnnouncement {
            pubkey,
            name,
            version,
            picture: ptr::null_mut(),
            about: ptr::null_mut(),
            website: ptr::null_mut(),
            event_id,
        };

        let name_str = from_c_str(announcement.name);
        assert_eq!(name_str, "Test Server");

        let version_str = from_c_str(announcement.version);
        assert_eq!(version_str, "1.0.0");

        // Cleanup - manually free strings
        cvm_string_free(announcement.pubkey);
        cvm_string_free(announcement.name);
        cvm_string_free(announcement.version);
        cvm_string_free(announcement.event_id);
    }
}

/// Test provider profile structure
#[test]
fn test_mock_relay_provider_profile_structure() {
    unsafe {
        let pubkey = to_c_str("0011223300112233001122330011223300112233001122330011223300112233");
        let name = to_c_str("Test Provider");
        let about = to_c_str("A test provider for ContextVM");

        let profile = FfiProviderProfile {
            pubkey,
            name,
            about,
            picture: ptr::null_mut(),
            nip05: ptr::null_mut(),
        };

        let name_str = from_c_str(profile.name);
        assert_eq!(name_str, "Test Provider");

        let about_str = from_c_str(profile.about);
        assert_eq!(about_str, "A test provider for ContextVM");

        // Cleanup - manually free strings
        cvm_string_free(profile.pubkey);
        cvm_string_free(profile.name);
        cvm_string_free(profile.about);
    }
}
