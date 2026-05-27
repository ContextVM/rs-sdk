//! Channel-based wrapper types that allow FFI consumers to receive messages.

use crate::builders::{build_sdk_client_config, build_sdk_server_config};
use crate::error::{set_error, FfiError};
use crate::handle::FfiHandle;
use crate::kv;
use crate::runtime::global_runtime;
use crate::types::*;

use std::os::raw::c_char;
use std::time::Duration;
use tokio::sync::Mutex;

// ─── Wrappers that combine transport + receiver ────────────────────────

/// Server wrapper holding transport + message receiver.
pub struct ServerChannel {
    transport: Mutex<contextvm_sdk::NostrServerTransport>,
    receiver: Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::IncomingRequest>>,
}

/// Client wrapper holding transport + message receiver.
pub struct ClientChannel {
    transport: Mutex<contextvm_sdk::NostrClientTransport>,
    receiver: Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::JsonRpcMessage>>,
}

/// Gateway wrapper holding gateway + message receiver.
pub struct GatewayChannel {
    gateway: Mutex<contextvm_sdk::gateway::NostrMCPGateway>,
    receiver: Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::IncomingRequest>>,
}

/// Proxy wrapper holding proxy + message receiver.
pub struct ProxyChannel {
    proxy: Mutex<contextvm_sdk::proxy::NostrMCPProxy>,
    receiver: Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::JsonRpcMessage>>,
}

// ─── Server Channel API ────────────────────────────────────────────────

/// Create and start a server transport with a channel receiver.
#[no_mangle]
pub extern "C" fn cvm_server_ch_new(
    keys_handle: FfiHandle,
    config: FfiServerConfig,
    error: *mut *mut FfiError,
) -> FfiHandle {
    let keys = match get_keys(keys_handle, error) {
        Some(k) => k,
        None => return FfiHandle { id: 0 },
    };

    let sdk_config = build_sdk_server_config(&config);

    let result = global_runtime().block_on(async {
        let mut transport = contextvm_sdk::NostrServerTransport::new(keys, sdk_config).await?;
        transport.start().await?;
        let receiver = transport
            .take_message_receiver()
            .ok_or_else(|| contextvm_sdk::Error::Other("receiver already taken".into()))?;
        Ok::<_, contextvm_sdk::Error>(ServerChannel {
            transport: Mutex::new(transport),
            receiver: Mutex::new(receiver),
        })
    });

    match result {
        Ok(ch) => kv::insert(ch),
        Err(e) => {
            set_error(error, e.into());
            FfiHandle { id: 0 }
        }
    }
}

/// Receive the next incoming request.  Blocks until available.
#[no_mangle]
pub extern "C" fn cvm_server_ch_recv(
    handle: FfiHandle,
    out_req: *mut FfiIncomingRequest,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid server channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut receiver = channel.receiver.lock().await;
        receiver.recv().await
    }) {
        Some(incoming) => {
            if !out_req.is_null() {
                unsafe {
                    *out_req = FfiIncomingRequest {
                        message: message_to_ffi(&incoming.message),
                        client_pubkey: string_to_c(incoming.client_pubkey),
                        event_id: string_to_c(incoming.event_id),
                        is_encrypted: incoming.is_encrypted,
                    };
                }
            }
            true
        }
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Transport,
                    message: "channel closed".into(),
                },
            );
            false
        }
    }
}

/// Send a response through a server channel.
#[no_mangle]
pub extern "C" fn cvm_server_ch_send_response(
    handle: FfiHandle,
    event_id: *const c_char,
    payload_json: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let guard = kv::get::<ServerChannel>(handle);
    let channel = match guard {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid server channel handle".into(),
                },
            );
            return false;
        }
    };

    let eid = match c_str_to_string(event_id) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Validation,
                    message: "null event_id".into(),
                },
            );
            return false;
        }
    };

    let msg = match parse_json_rpc_message(payload_json, error) {
        Some(m) => m,
        None => return false,
    };

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.send_response(&eid, msg).await
    }) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Publish server announcement.
#[no_mangle]
pub extern "C" fn cvm_server_ch_announce(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let guard = kv::get::<ServerChannel>(handle);
    let channel = match guard {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid server channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.announce().await
    }) {
        Ok(_) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Close a server channel.
#[no_mangle]
pub extern "C" fn cvm_server_ch_close(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid server channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut transport = channel.transport.lock().await;
        transport.close().await
    }) {
        Ok(()) => {
            kv::remove(handle);
            true
        }
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

// ─── Client Channel API ────────────────────────────────────────────────

/// Create and start a client transport with a channel receiver.
#[no_mangle]
pub extern "C" fn cvm_client_ch_new(
    keys_handle: FfiHandle,
    config: FfiClientConfig,
    error: *mut *mut FfiError,
) -> FfiHandle {
    let keys = match get_keys(keys_handle, error) {
        Some(k) => k,
        None => return FfiHandle { id: 0 },
    };

    let sdk_config = match build_sdk_client_config(&config) {
        Some(c) => c,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Validation,
                    message: "server_pubkey is required".into(),
                },
            );
            return FfiHandle { id: 0 };
        }
    };

    let result = global_runtime().block_on(async {
        let mut transport = contextvm_sdk::NostrClientTransport::new(keys, sdk_config).await?;
        transport.start().await?;
        let receiver = transport
            .take_message_receiver()
            .ok_or_else(|| contextvm_sdk::Error::Other("receiver already taken".into()))?;
        Ok::<_, contextvm_sdk::Error>(ClientChannel {
            transport: Mutex::new(transport),
            receiver: Mutex::new(receiver),
        })
    });

    match result {
        Ok(ch) => kv::insert(ch),
        Err(e) => {
            set_error(error, e.into());
            FfiHandle { id: 0 }
        }
    }
}

/// Send a message through a client channel.
#[no_mangle]
pub extern "C" fn cvm_client_ch_send(
    handle: FfiHandle,
    payload_json: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let guard = kv::get::<ClientChannel>(handle);
    let channel = match guard {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid client channel handle".into(),
                },
            );
            return false;
        }
    };

    let msg = match parse_json_rpc_message(payload_json, error) {
        Some(m) => m,
        None => return false,
    };

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.send(&msg).await
    }) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Receive the next message.  Blocks until available.
#[no_mangle]
pub extern "C" fn cvm_client_ch_recv(
    handle: FfiHandle,
    out_msg: *mut FfiJsonRpcMessage,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ClientChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid client channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut receiver = channel.receiver.lock().await;
        receiver.recv().await
    }) {
        Some(message) => {
            if !out_msg.is_null() {
                unsafe {
                    *out_msg = message_to_ffi(&message);
                }
            }
            true
        }
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Transport,
                    message: "channel closed".into(),
                },
            );
            false
        }
    }
}

/// Close a client channel.
#[no_mangle]
pub extern "C" fn cvm_client_ch_close(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let channel = match kv::get::<ClientChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid client channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut transport = channel.transport.lock().await;
        transport.close().await
    }) {
        Ok(()) => {
            kv::remove(handle);
            true
        }
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

// ─── Gateway Channel API ───────────────────────────────────────────────

/// Create and start a gateway with a channel receiver.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_new(
    keys_handle: FfiHandle,
    config: FfiServerConfig,
    error: *mut *mut FfiError,
) -> FfiHandle {
    let keys = match get_keys(keys_handle, error) {
        Some(k) => k,
        None => return FfiHandle { id: 0 },
    };

    let sdk_config = build_sdk_server_config(&config);
    let gw_config = contextvm_sdk::gateway::GatewayConfig::new(sdk_config);

    let result = global_runtime().block_on(async {
        let mut gw = contextvm_sdk::gateway::NostrMCPGateway::new(keys, gw_config).await?;
        let receiver = gw.start().await?;
        Ok::<_, contextvm_sdk::Error>(GatewayChannel {
            gateway: Mutex::new(gw),
            receiver: Mutex::new(receiver),
        })
    });

    match result {
        Ok(ch) => kv::insert(ch),
        Err(e) => {
            set_error(error, e.into());
            FfiHandle { id: 0 }
        }
    }
}

/// Receive the next request from a gateway channel.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_recv(
    handle: FfiHandle,
    out_req: *mut FfiIncomingRequest,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<GatewayChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid gateway channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut receiver = channel.receiver.lock().await;
        receiver.recv().await
    }) {
        Some(incoming) => {
            if !out_req.is_null() {
                unsafe {
                    *out_req = FfiIncomingRequest {
                        message: message_to_ffi(&incoming.message),
                        client_pubkey: string_to_c(incoming.client_pubkey),
                        event_id: string_to_c(incoming.event_id),
                        is_encrypted: incoming.is_encrypted,
                    };
                }
            }
            true
        }
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Transport,
                    message: "channel closed".into(),
                },
            );
            false
        }
    }
}

/// Send a response through a gateway channel.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_send_response(
    handle: FfiHandle,
    event_id: *const c_char,
    payload_json: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let guard = kv::get::<GatewayChannel>(handle);
    let channel = match guard {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid gateway channel handle".into(),
                },
            );
            return false;
        }
    };

    let eid = match c_str_to_string(event_id) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Validation,
                    message: "null event_id".into(),
                },
            );
            return false;
        }
    };

    let msg = match parse_json_rpc_message(payload_json, error) {
        Some(m) => m,
        None => return false,
    };

    match global_runtime().block_on(async {
        let gateway = channel.gateway.lock().await;
        gateway.send_response(&eid, msg).await
    }) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Publish announcement through a gateway channel.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_announce(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let guard = kv::get::<GatewayChannel>(handle);
    let channel = match guard {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid gateway channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let gateway = channel.gateway.lock().await;
        gateway.announce().await
    }) {
        Ok(_) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Stop a gateway channel.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_stop(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let channel = match kv::get::<GatewayChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid gateway channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut gateway = channel.gateway.lock().await;
        gateway.stop().await
    }) {
        Ok(()) => {
            kv::remove(handle);
            true
        }
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

// ─── Proxy Channel API ─────────────────────────────────────────────────

/// Create and start a proxy with a channel receiver.
#[no_mangle]
pub extern "C" fn cvm_proxy_ch_new(
    keys_handle: FfiHandle,
    config: FfiClientConfig,
    error: *mut *mut FfiError,
) -> FfiHandle {
    let keys = match get_keys(keys_handle, error) {
        Some(k) => k,
        None => return FfiHandle { id: 0 },
    };

    let sdk_config = match build_sdk_client_config(&config) {
        Some(c) => c,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Validation,
                    message: "server_pubkey is required".into(),
                },
            );
            return FfiHandle { id: 0 };
        }
    };

    let proxy_config = contextvm_sdk::proxy::ProxyConfig::new(sdk_config);

    let result = global_runtime().block_on(async {
        let mut proxy = contextvm_sdk::proxy::NostrMCPProxy::new(keys, proxy_config).await?;
        let receiver = proxy.start().await?;
        Ok::<_, contextvm_sdk::Error>(ProxyChannel {
            proxy: Mutex::new(proxy),
            receiver: Mutex::new(receiver),
        })
    });

    match result {
        Ok(ch) => kv::insert(ch),
        Err(e) => {
            set_error(error, e.into());
            FfiHandle { id: 0 }
        }
    }
}

/// Send a message through a proxy channel.
#[no_mangle]
pub extern "C" fn cvm_proxy_ch_send(
    handle: FfiHandle,
    payload_json: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let guard = kv::get::<ProxyChannel>(handle);
    let channel = match guard {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid proxy channel handle".into(),
                },
            );
            return false;
        }
    };

    let msg = match parse_json_rpc_message(payload_json, error) {
        Some(m) => m,
        None => return false,
    };

    match global_runtime().block_on(async {
        let proxy = channel.proxy.lock().await;
        proxy.send(&msg).await
    }) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Receive the next message from a proxy channel.
#[no_mangle]
pub extern "C" fn cvm_proxy_ch_recv(
    handle: FfiHandle,
    out_msg: *mut FfiJsonRpcMessage,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ProxyChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid proxy channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut receiver = channel.receiver.lock().await;
        receiver.recv().await
    }) {
        Some(message) => {
            if !out_msg.is_null() {
                unsafe {
                    *out_msg = message_to_ffi(&message);
                }
            }
            true
        }
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Transport,
                    message: "channel closed".into(),
                },
            );
            false
        }
    }
}

/// Receive the next message from a proxy channel, timing out after `timeout_secs`.
#[no_mangle]
pub extern "C" fn cvm_proxy_ch_recv_timeout(
    handle: FfiHandle,
    timeout_secs: u64,
    out_msg: *mut FfiJsonRpcMessage,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ProxyChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid proxy channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut receiver = channel.receiver.lock().await;
        tokio::time::timeout(Duration::from_secs(timeout_secs), receiver.recv()).await
    }) {
        Ok(Some(message)) => {
            if !out_msg.is_null() {
                unsafe {
                    *out_msg = message_to_ffi(&message);
                }
            }
            true
        }
        Ok(None) => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Transport,
                    message: "channel closed".into(),
                },
            );
            false
        }
        Err(_) => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Timeout,
                    message: "receive timed out".into(),
                },
            );
            false
        }
    }
}

/// Stop a proxy channel.
#[no_mangle]
pub extern "C" fn cvm_proxy_ch_stop(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let channel = match kv::get::<ProxyChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid proxy channel handle".into(),
                },
            );
            return false;
        }
    };

    match global_runtime().block_on(async {
        let mut proxy = channel.proxy.lock().await;
        proxy.stop().await
    }) {
        Ok(()) => {
            kv::remove(handle);
            true
        }
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────

fn get_keys(handle: FfiHandle, error: *mut *mut FfiError) -> Option<contextvm_sdk::signer::Keys> {
    match kv::get::<contextvm_sdk::signer::Keys>(handle) {
        Some(k) => Some(k.as_ref().clone()),
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "invalid key handle".into(),
                },
            );
            None
        }
    }
}

fn parse_json_rpc_message(
    payload_json: *const c_char,
    error: *mut *mut FfiError,
) -> Option<contextvm_sdk::JsonRpcMessage> {
    let json_str = match c_str_to_string(payload_json) {
        Some(s) => s,
        None => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Validation,
                    message: "null payload json".into(),
                },
            );
            return None;
        }
    };

    match serde_json::from_str(&json_str) {
        Ok(m) => Some(m),
        Err(e) => {
            set_error(
                error,
                FfiError {
                    code: crate::error::ErrorCode::Serialization,
                    message: e.to_string(),
                },
            );
            None
        }
    }
}
