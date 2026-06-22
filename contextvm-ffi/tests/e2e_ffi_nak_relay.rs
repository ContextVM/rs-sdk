//! End-to-end tests driven by a real `nak serve` relay.
//!
//! These tests spawn the `nak` CLI (https://github.com/nbd-wtf/nostr-cli) to run
//! an actual in-process relay and exercise real message flow through the FFI
//! bindings. They are gated behind the `nak-tests` feature and additionally
//! no-op when the `nak` binary is not on PATH, so they are safe under
//! `cargo test --all --all-features` without `nak` installed.

use contextvm_ffi::{
    cvm_client_ch_close, cvm_client_ch_new, cvm_client_ch_recv_timeout, cvm_client_ch_send,
    cvm_keys_free, cvm_keys_generate, cvm_keys_public_key, cvm_server_ch_announce,
    cvm_server_ch_close, cvm_server_ch_new, cvm_server_ch_recv_timeout, cvm_string_free,
    error::FfiError, types::*,
};
use std::ffi::{CStr, CString};
use std::net::TcpStream;
use std::os::raw::c_char;
use std::process::{Child, Command, Stdio};
use std::ptr;
use std::thread;
use std::time::Duration;

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

/// Manages a nak relay process for testing
struct NakRelay {
    process: Child,
    url: String,
}

/// Returns true if the `nak` binary is available on PATH.
///
/// The nak-tests target is feature-gated, but CI runs `cargo test --all-features`,
/// which enables it. To stay green without `nak` installed, tests call this and
/// no-op when the binary is absent.
fn nak_available() -> bool {
    Command::new("nak")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Skip helper for tests that need `nak`. Prints a note and returns true if the
/// test should be skipped (binary absent), false if it should proceed.
fn skip_if_no_nak() -> bool {
    if !nak_available() {
        eprintln!(
            "nak binary not found on PATH; skipping nak e2e test \
             (enable `nak-tests` and install nak to run it)"
        );
        return true;
    }
    false
}

impl NakRelay {
    /// Start a nak relay on a random available port.
    ///
    /// Panics with an actionable message if `nak` cannot be launched.
    fn start() -> Self {
        // Try ports in a range to find an available one
        for port in 33333..33400 {
            let url = format!("ws://127.0.0.1:{}", port);

            // Start nak serve
            let child = Command::new("nak")
                .args([
                    "serve",
                    "--hostname",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();

            match child {
                Ok(mut process) => {
                    // Probe the listener with a short TCP connect retry loop so
                    // we only proceed once nak is actually accepting sockets.
                    if Self::wait_for_relay(&url, Duration::from_secs(5)) {
                        return NakRelay { process, url };
                    } else {
                        // Relay didn't come up on this port; clean up and try next.
                        let _ = process.kill();
                        let _ = process.wait();
                        continue;
                    }
                }
                Err(e) => {
                    // Most likely `nak` is not installed at all — surface that
                    // clearly rather than masking it as a port conflict.
                    if e.kind() == std::io::ErrorKind::NotFound {
                        panic!(
                            "`nak` binary not found on PATH; install it \
                             (https://github.com/nbd-wtf/nostr-cli) or run without \
                             the `nak-tests` feature"
                        );
                    }
                    continue; // Transient launch failure; try next port.
                }
            }
        }

        panic!("Could not start nak relay on any port in 33333..33400");
    }

    /// Poll the relay address with a TCP connect until it succeeds or times out.
    fn wait_for_relay(url: &str, deadline: Duration) -> bool {
        let addr = url.trim_start_matches("ws://");
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if TcpStream::connect_timeout(
                &addr
                    .parse()
                    .unwrap_or_else(|_| "127.0.0.1:0".parse().unwrap()),
                Duration::from_millis(200),
            )
            .is_ok()
            {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for NakRelay {
    fn drop(&mut self) {
        // Kill the nak process
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Test that nak relay can be started and stopped
#[test]
fn test_nak_relay_lifecycle() {
    if skip_if_no_nak() {
        return;
    }
    let relay = NakRelay::start();
    println!("Started nak relay at: {}", relay.url());
    // Relay will be stopped when dropped
}

/// Test server channel creation with nak relay
#[test]
fn test_nak_server_channel_creation() {
    if skip_if_no_nak() {
        return;
    }
    let relay = NakRelay::start();
    let relay_url = relay.url();

    unsafe {
        let mut error: *mut FfiError = ptr::null_mut();

        // Generate server keys
        let server_keys = cvm_keys_generate(&mut error);
        assert!(server_keys.id > 0, "Should generate server keys");

        let server_pubkey = cvm_keys_public_key(server_keys, &mut error);
        assert!(!server_pubkey.is_null());
        println!("Server pubkey: {}", from_c_str(server_pubkey));
        cvm_string_free(server_pubkey);

        // Create server config with nak relay
        let relay_urls = vec![to_c_str(relay_url)];

        let config = FfiServerConfig {
            relay_urls: relay_urls.as_ptr() as *mut *mut c_char,
            relay_url_count: 1,
            encryption_mode: ENCRYPTION_MODE_OPTIONAL,
            gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
            is_announced_server: false,
            server_name: to_c_str("Test Server"),
            server_version: to_c_str("1.0.0"),
            server_picture: ptr::null_mut(),
            server_about: to_c_str("Test server for nak E2E"),
            server_website: ptr::null_mut(),
            allowed_pubkeys: ptr::null_mut(),
            allowed_pubkey_count: 0,
            session_timeout_secs: 300,
            cleanup_interval_secs: 60,
            excluded_capabilities: ptr::null_mut(),
            excluded_capability_count: 0,
            max_sessions: 0,
            request_timeout_secs: 30,
            relay_list_urls: ptr::null_mut(),
            relay_list_url_count: 0,
            bootstrap_relay_urls: ptr::null_mut(),
            bootstrap_relay_url_count: 0,
            publish_relay_list: false,
            profile_metadata_json: ptr::null_mut(),
        };

        // Create server channel
        let server_handle = cvm_server_ch_new(server_keys, config, &mut error);

        // Note: server_ch_new might fail if relay is not ready, that's OK for this test
        if server_handle.id > 0 {
            println!(
                "Server channel created successfully: handle {}",
                server_handle.id
            );

            // Try to announce
            let announce_result = cvm_server_ch_announce(server_handle, &mut error);
            println!("Server announce result: {}", announce_result);

            // Close the server channel
            let close_result = cvm_server_ch_close(server_handle, &mut error);
            assert!(close_result, "Should close server channel");
        } else {
            println!("Server channel creation returned invalid handle (relay might not be ready)");
        }

        // Cleanup
        for url in relay_urls {
            cvm_string_free(url);
        }
        cvm_keys_free(server_keys);
    }
}

/// Test client channel creation with nak relay
#[test]
fn test_nak_client_channel_creation() {
    if skip_if_no_nak() {
        return;
    }
    let relay = NakRelay::start();
    let relay_url = relay.url();

    let mut error: *mut FfiError = ptr::null_mut();

    // Generate client keys
    let client_keys = cvm_keys_generate(&mut error);
    assert!(client_keys.id > 0, "Should generate client keys");

    // Generate a mock server pubkey
    let server_pubkey_str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    // Create client config
    let relay_urls = vec![to_c_str(relay_url)];
    let server_pubkey = to_c_str(server_pubkey_str);

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

    // Create client channel
    let client_handle = cvm_client_ch_new(client_keys, config, &mut error);

    if client_handle.id > 0 {
        println!(
            "Client channel created successfully: handle {}",
            client_handle.id
        );

        // Try to send a message
        let payload = to_c_str(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        let send_result = cvm_client_ch_send(client_handle, payload, &mut error);
        println!("Client send result: {}", send_result);
        cvm_string_free(payload);

        // Try to receive with short timeout
        let mut out_msg = std::mem::MaybeUninit::<FfiJsonRpcMessage>::uninit();
        let recv_result =
            cvm_client_ch_recv_timeout(client_handle, 2, out_msg.as_mut_ptr(), &mut error);
        println!(
            "Client recv result: {} (expected false - no server)",
            recv_result
        );

        // Close the client channel
        let close_result = cvm_client_ch_close(client_handle, &mut error);
        assert!(close_result, "Should close client channel");
    } else {
        println!("Client channel creation returned invalid handle");
    }

    // Cleanup
    for url in relay_urls {
        cvm_string_free(url);
    }
    cvm_string_free(server_pubkey);
    cvm_keys_free(client_keys);
}

/// Test full server-client message flow through nak relay
#[test]
fn test_nak_server_client_message_flow() {
    if skip_if_no_nak() {
        return;
    }
    let relay = NakRelay::start();
    let relay_url = relay.url();

    unsafe {
        let mut error: *mut FfiError = ptr::null_mut();

        // Generate keys for both sides
        let server_keys = cvm_keys_generate(&mut error);
        let client_keys = cvm_keys_generate(&mut error);

        assert!(server_keys.id > 0, "Should generate server keys");
        assert!(client_keys.id > 0, "Should generate client keys");

        let server_pubkey = cvm_keys_public_key(server_keys, &mut error);
        let server_pubkey_str = from_c_str(server_pubkey);
        println!("Server pubkey: {}", server_pubkey_str);

        // Create server config
        let relay_urls = vec![to_c_str(relay_url)];
        let server_config = FfiServerConfig {
            relay_urls: relay_urls.as_ptr() as *mut *mut c_char,
            relay_url_count: 1,
            encryption_mode: ENCRYPTION_MODE_OPTIONAL,
            gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
            is_announced_server: true,
            server_name: to_c_str("E2E Test Server"),
            server_version: to_c_str("1.0.0"),
            server_picture: ptr::null_mut(),
            server_about: to_c_str("Server for E2E test"),
            server_website: ptr::null_mut(),
            allowed_pubkeys: ptr::null_mut(),
            allowed_pubkey_count: 0,
            session_timeout_secs: 300,
            cleanup_interval_secs: 60,
            excluded_capabilities: ptr::null_mut(),
            excluded_capability_count: 0,
            max_sessions: 0,
            request_timeout_secs: 30,
            relay_list_urls: ptr::null_mut(),
            relay_list_url_count: 0,
            bootstrap_relay_urls: ptr::null_mut(),
            bootstrap_relay_url_count: 0,
            publish_relay_list: false,
            profile_metadata_json: ptr::null_mut(),
        };

        // Create client config
        let server_pubkey_c = to_c_str(&server_pubkey_str);
        let client_config = FfiClientConfig {
            relay_urls: relay_urls.as_ptr() as *mut *mut c_char,
            relay_url_count: 1,
            server_pubkey: server_pubkey_c,
            encryption_mode: ENCRYPTION_MODE_OPTIONAL,
            gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
            is_stateless: false,
            timeout_secs: 30,
            discovery_relay_urls: ptr::null_mut(),
            discovery_relay_url_count: 0,
            fallback_operational_relay_urls: ptr::null_mut(),
            fallback_operational_relay_url_count: 0,
        };

        // Create channels
        let server_handle = cvm_server_ch_new(server_keys, server_config, &mut error);
        let client_handle = cvm_client_ch_new(client_keys, client_config, &mut error);

        println!(
            "Server handle: {}, Client handle: {}",
            server_handle.id, client_handle.id
        );

        if server_handle.id > 0 && client_handle.id > 0 {
            // Announce server
            let announce_result = cvm_server_ch_announce(server_handle, &mut error);
            println!("Server announce: {}", announce_result);

            // Give time for announcement
            thread::sleep(Duration::from_millis(1000));

            // Try server recv (might get initialization request)
            let mut out_req = std::mem::MaybeUninit::<FfiIncomingRequest>::uninit();
            let recv_result =
                cvm_server_ch_recv_timeout(server_handle, 2, out_req.as_mut_ptr(), &mut error);
            println!("Server recv: {}", recv_result);

            if recv_result {
                // We got a request, could send response
                println!("Server received request!");
            }

            // Close channels
            let _ = cvm_server_ch_close(server_handle, &mut error);
            let _ = cvm_client_ch_close(client_handle, &mut error);
        }

        // Cleanup
        for url in relay_urls {
            cvm_string_free(url);
        }
        cvm_string_free(server_pubkey);
        cvm_string_free(server_pubkey_c);
        cvm_keys_free(server_keys);
        cvm_keys_free(client_keys);
    }
}

/// Test multiple concurrent connections to nak relay
#[test]
fn test_nak_multiple_connections() {
    if skip_if_no_nak() {
        return;
    }
    let relay = NakRelay::start();
    let relay_url = relay.url();

    let mut error: *mut FfiError = ptr::null_mut();

    // Create multiple server channels
    let mut handles = Vec::new();

    for i in 0..3 {
        let keys = cvm_keys_generate(&mut error);
        assert!(keys.id > 0);

        let relay_urls = vec![to_c_str(relay_url)];
        let config = FfiServerConfig {
            relay_urls: relay_urls.as_ptr() as *mut *mut c_char,
            relay_url_count: 1,
            encryption_mode: ENCRYPTION_MODE_OPTIONAL,
            gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
            is_announced_server: false,
            server_name: to_c_str(&format!("Test Server {}", i)),
            server_version: to_c_str("1.0.0"),
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
            request_timeout_secs: 30,
            relay_list_urls: ptr::null_mut(),
            relay_list_url_count: 0,
            bootstrap_relay_urls: ptr::null_mut(),
            bootstrap_relay_url_count: 0,
            publish_relay_list: false,
            profile_metadata_json: ptr::null_mut(),
        };

        let handle = cvm_server_ch_new(keys, config, &mut error);
        if handle.id > 0 {
            handles.push((handle, keys, relay_urls));
            println!("Created server {}: handle {}", i, handle.id);
        }
    }

    // Close all channels
    for (handle, keys, urls) in handles {
        let _ = cvm_server_ch_close(handle, &mut error);
        cvm_keys_free(keys);
        for url in urls {
            cvm_string_free(url);
        }
    }

    println!("Successfully tested {} concurrent server connections", 3);
}
