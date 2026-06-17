//! Channel-based wrapper types that allow FFI consumers to receive messages.

use crate::builders::{build_sdk_client_config, build_sdk_server_config};
use crate::error::{set_error, ErrorCode, FfiError};
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

    let sdk_config = match build_sdk_server_config(&config) {
        Ok(config) => config,
        Err(e) => {
            set_error(error, e);
            return FfiHandle { id: 0 };
        }
    };

    let result = global_runtime().block_on(async {
        let mut transport = contextvm_sdk::NostrServerTransport::new(keys, sdk_config).await?;
        transport.start().await?;
        transport.spawn_discoverability_publication();
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

/// Receive the next incoming request, timing out after `timeout_secs`.
#[no_mangle]
pub extern "C" fn cvm_server_ch_recv_timeout(
    handle: FfiHandle,
    timeout_secs: u64,
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
        tokio::time::timeout(Duration::from_secs(timeout_secs), receiver.recv()).await
    }) {
        Ok(Some(incoming)) => {
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

/// Send a notification to a specific client through a server channel.
#[no_mangle]
pub extern "C" fn cvm_server_ch_send_notification(
    handle: FfiHandle,
    client_pubkey: *const c_char,
    payload_json: *const c_char,
    correlated_event_id: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "server channel");
            return false;
        }
    };

    let client_pubkey = match c_str_to_string(client_pubkey) {
        Some(s) => s,
        None => {
            set_null_arg_error(error, "client_pubkey");
            return false;
        }
    };
    let msg = match parse_json_rpc_message(payload_json, error) {
        Some(m) => m,
        None => return false,
    };
    let correlated_event_id = c_str_to_string(correlated_event_id);

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport
            .send_notification(&client_pubkey, &msg, correlated_event_id.as_deref())
            .await
    }) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Broadcast a notification to all initialized clients.
#[no_mangle]
pub extern "C" fn cvm_server_ch_broadcast_notification(
    handle: FfiHandle,
    payload_json: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "server channel");
            return false;
        }
    };

    let msg = match parse_json_rpc_message(payload_json, error) {
        Some(m) => m,
        None => return false,
    };

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.broadcast_notification(&msg).await
    }) {
        Ok(()) => true,
        Err(e) => {
            set_error(error, e.into());
            false
        }
    }
}

/// Sets extra announcement/discovery tags from a JSON array of tag arrays.
#[no_mangle]
pub extern "C" fn cvm_server_ch_set_announcement_extra_tags(
    handle: FfiHandle,
    tags_json: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "server channel");
            return false;
        }
    };
    let tags = match parse_tags_json(tags_json, error) {
        Some(tags) => tags,
        None => return false,
    };

    global_runtime().block_on(async {
        let mut transport = channel.transport.lock().await;
        transport.set_announcement_extra_tags(tags);
    });
    true
}

/// Sets pricing tags from a JSON array of tag arrays.
#[no_mangle]
pub extern "C" fn cvm_server_ch_set_announcement_pricing_tags(
    handle: FfiHandle,
    tags_json: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "server channel");
            return false;
        }
    };
    let tags = match parse_tags_json(tags_json, error) {
        Some(tags) => tags,
        None => return false,
    };

    global_runtime().block_on(async {
        let mut transport = channel.transport.lock().await;
        transport.set_announcement_pricing_tags(tags);
    });
    true
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

/// Publish server announcement and return the Nostr event ID.
#[no_mangle]
pub extern "C" fn cvm_server_ch_announce_event_id(
    handle: FfiHandle,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "server channel");
            return std::ptr::null_mut();
        }
    };

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.announce().await
    }) {
        Ok(event_id) => string_to_c(event_id.to_hex()),
        Err(e) => {
            set_error(error, e.into());
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn cvm_server_ch_publish_tools(
    handle: FfiHandle,
    tools_json: *const c_char,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let tools = match parse_json_value_array(tools_json, "tools_json", error) {
        Some(values) => values,
        None => return std::ptr::null_mut(),
    };
    publish_server_values(handle, tools, ServerPublishListKind::Tools, error)
}

#[no_mangle]
pub extern "C" fn cvm_server_ch_publish_resources(
    handle: FfiHandle,
    resources_json: *const c_char,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let resources = match parse_json_value_array(resources_json, "resources_json", error) {
        Some(values) => values,
        None => return std::ptr::null_mut(),
    };
    publish_server_values(handle, resources, ServerPublishListKind::Resources, error)
}

#[no_mangle]
pub extern "C" fn cvm_server_ch_publish_prompts(
    handle: FfiHandle,
    prompts_json: *const c_char,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let prompts = match parse_json_value_array(prompts_json, "prompts_json", error) {
        Some(values) => values,
        None => return std::ptr::null_mut(),
    };
    publish_server_values(handle, prompts, ServerPublishListKind::Prompts, error)
}

#[no_mangle]
pub extern "C" fn cvm_server_ch_publish_resource_templates(
    handle: FfiHandle,
    templates_json: *const c_char,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let templates = match parse_json_value_array(templates_json, "templates_json", error) {
        Some(values) => values,
        None => return std::ptr::null_mut(),
    };
    publish_server_values(
        handle,
        templates,
        ServerPublishListKind::ResourceTemplates,
        error,
    )
}

#[no_mangle]
pub extern "C" fn cvm_server_ch_delete_announcements(
    handle: FfiHandle,
    reason: *const c_char,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "server channel");
            return false;
        }
    };
    let reason = match c_str_to_string(reason) {
        Some(s) => s,
        None => {
            set_null_arg_error(error, "reason");
            return false;
        }
    };

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.delete_announcements(&reason).await
    }) {
        Ok(()) => true,
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
        Ok(c) => c,
        Err(e) => {
            set_error(error, e);
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

/// Receive the next message from a client channel, timing out after `timeout_secs`.
#[no_mangle]
pub extern "C" fn cvm_client_ch_recv_timeout(
    handle: FfiHandle,
    timeout_secs: u64,
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

/// Return a snapshot of server capabilities learned from discovery tags.
#[no_mangle]
pub extern "C" fn cvm_client_ch_discovered_server_capabilities(
    handle: FfiHandle,
    out_caps: *mut FfiPeerCapabilities,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ClientChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "client channel");
            return false;
        }
    };
    if out_caps.is_null() {
        set_null_arg_error(error, "out_caps");
        return false;
    }

    let caps = global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.discovered_server_capabilities()
    });
    unsafe {
        *out_caps = peer_capabilities_to_ffi(caps);
    }
    true
}

/// Return whether the client has learned ephemeral gift-wrap support.
#[no_mangle]
pub extern "C" fn cvm_client_ch_server_supports_ephemeral_encryption(
    handle: FfiHandle,
    error: *mut *mut FfiError,
) -> bool {
    let channel = match kv::get::<ClientChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "client channel");
            return false;
        }
    };

    global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.server_supports_ephemeral_encryption()
    })
}

/// Return the first server event carrying discovery tags as JSON, or NULL if none.
#[no_mangle]
pub extern "C" fn cvm_client_ch_server_initialize_event_json(
    handle: FfiHandle,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let channel = match kv::get::<ClientChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "client channel");
            return std::ptr::null_mut();
        }
    };

    let event = global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        transport.get_server_initialize_event()
    });

    match event {
        Some(event) => match serde_json::to_string(&event) {
            Ok(json) => string_to_c(json),
            Err(e) => {
                set_error(
                    error,
                    FfiError {
                        code: ErrorCode::Serialization,
                        message: e.to_string(),
                    },
                );
                std::ptr::null_mut()
            }
        },
        None => std::ptr::null_mut(),
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

    let sdk_config = match build_sdk_server_config(&config) {
        Ok(config) => config,
        Err(e) => {
            set_error(error, e);
            return FfiHandle { id: 0 };
        }
    };
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

/// Receive the next request from a gateway channel, timing out after `timeout_secs`.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_recv_timeout(
    handle: FfiHandle,
    timeout_secs: u64,
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
        tokio::time::timeout(Duration::from_secs(timeout_secs), receiver.recv()).await
    }) {
        Ok(Some(incoming)) => {
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

/// Publish gateway server announcement and return the Nostr event ID.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_announce_event_id(
    handle: FfiHandle,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let channel = match kv::get::<GatewayChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "gateway channel");
            return std::ptr::null_mut();
        }
    };

    match global_runtime().block_on(async {
        let gateway = channel.gateway.lock().await;
        gateway.announce().await
    }) {
        Ok(event_id) => string_to_c(event_id.to_hex()),
        Err(e) => {
            set_error(error, e.into());
            std::ptr::null_mut()
        }
    }
}

/// Check if a gateway channel is active.
#[no_mangle]
pub extern "C" fn cvm_gateway_ch_is_active(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let channel = match kv::get::<GatewayChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "gateway channel");
            return false;
        }
    };

    global_runtime().block_on(async {
        let gateway = channel.gateway.lock().await;
        gateway.is_active()
    })
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
        Ok(c) => c,
        Err(e) => {
            set_error(error, e);
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

/// Check if a proxy channel is active.
#[no_mangle]
pub extern "C" fn cvm_proxy_ch_is_active(handle: FfiHandle, error: *mut *mut FfiError) -> bool {
    let channel = match kv::get::<ProxyChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "proxy channel");
            return false;
        }
    };

    global_runtime().block_on(async {
        let proxy = channel.proxy.lock().await;
        proxy.is_active()
    })
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

fn parse_json_value_array(
    payload_json: *const c_char,
    name: &str,
    error: *mut *mut FfiError,
) -> Option<Vec<serde_json::Value>> {
    let json_str = match c_str_to_string(payload_json) {
        Some(s) => s,
        None => {
            set_null_arg_error(error, name);
            return None;
        }
    };

    match serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
        Ok(values) => Some(values),
        Err(e) => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Serialization,
                    message: e.to_string(),
                },
            );
            None
        }
    }
}

fn parse_tags_json(
    tags_json: *const c_char,
    error: *mut *mut FfiError,
) -> Option<Vec<nostr_sdk::prelude::Tag>> {
    let json_str = match c_str_to_string(tags_json) {
        Some(s) => s,
        None => {
            set_null_arg_error(error, "tags_json");
            return None;
        }
    };

    let parts = match serde_json::from_str::<Vec<Vec<String>>>(&json_str) {
        Ok(parts) => parts,
        Err(e) => {
            set_error(
                error,
                FfiError {
                    code: ErrorCode::Serialization,
                    message: e.to_string(),
                },
            );
            return None;
        }
    };

    parts
        .into_iter()
        .map(|tag| {
            nostr_sdk::prelude::Tag::parse(tag).map_err(|e| FfiError {
                code: ErrorCode::Validation,
                message: e.to_string(),
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| set_error(error, e))
        .ok()
}

enum ServerPublishListKind {
    Tools,
    Resources,
    Prompts,
    ResourceTemplates,
}

fn publish_server_values(
    handle: FfiHandle,
    values: Vec<serde_json::Value>,
    kind: ServerPublishListKind,
    error: *mut *mut FfiError,
) -> *mut c_char {
    let channel = match kv::get::<ServerChannel>(handle) {
        Some(ch) => ch,
        None => {
            set_invalid_handle_error(error, "server channel");
            return std::ptr::null_mut();
        }
    };

    match global_runtime().block_on(async {
        let transport = channel.transport.lock().await;
        match kind {
            ServerPublishListKind::Tools => transport.publish_tools(values).await,
            ServerPublishListKind::Resources => transport.publish_resources(values).await,
            ServerPublishListKind::Prompts => transport.publish_prompts(values).await,
            ServerPublishListKind::ResourceTemplates => {
                transport.publish_resource_templates(values).await
            }
        }
    }) {
        Ok(event_id) => string_to_c(event_id.to_hex()),
        Err(e) => {
            set_error(error, e.into());
            std::ptr::null_mut()
        }
    }
}

fn set_invalid_handle_error(error: *mut *mut FfiError, handle_type: &str) {
    set_error(
        error,
        FfiError {
            code: ErrorCode::Other,
            message: format!("invalid {handle_type} handle"),
        },
    );
}

fn set_null_arg_error(error: *mut *mut FfiError, name: &str) {
    set_error(
        error,
        FfiError {
            code: ErrorCode::Validation,
            message: format!("null {name}"),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    mod timeout_tests {
        use super::*;

        #[test]
        fn test_server_recv_timeout_invalid_handle() {
            let invalid_handle = FfiHandle { id: 99999 };
            let mut out_req = std::mem::MaybeUninit::<FfiIncomingRequest>::uninit();
            let mut error: *mut FfiError = std::ptr::null_mut();

            let result =
                cvm_server_ch_recv_timeout(invalid_handle, 1, out_req.as_mut_ptr(), &mut error);

            assert!(!result, "Should return false for invalid handle");
            assert!(!error.is_null(), "Should set error for invalid handle");

            unsafe {
                assert_eq!((*error).code, ErrorCode::Other);
                crate::types::cvm_error_free(error);
            }
        }

        #[test]
        fn test_client_recv_timeout_invalid_handle() {
            let invalid_handle = FfiHandle { id: 99999 };
            let mut out_msg = std::mem::MaybeUninit::<FfiJsonRpcMessage>::uninit();
            let mut error: *mut FfiError = std::ptr::null_mut();

            let result =
                cvm_client_ch_recv_timeout(invalid_handle, 1, out_msg.as_mut_ptr(), &mut error);

            assert!(!result, "Should return false for invalid handle");
            assert!(!error.is_null(), "Should set error for invalid handle");

            unsafe {
                assert_eq!((*error).code, ErrorCode::Other);
                crate::types::cvm_error_free(error);
            }
        }

        #[test]
        fn test_gateway_recv_timeout_invalid_handle() {
            let invalid_handle = FfiHandle { id: 99999 };
            let mut out_req = std::mem::MaybeUninit::<FfiIncomingRequest>::uninit();
            let mut error: *mut FfiError = std::ptr::null_mut();

            let result =
                cvm_gateway_ch_recv_timeout(invalid_handle, 1, out_req.as_mut_ptr(), &mut error);

            assert!(!result, "Should return false for invalid handle");
            assert!(!error.is_null(), "Should set error for invalid handle");

            unsafe {
                assert_eq!((*error).code, ErrorCode::Other);
                crate::types::cvm_error_free(error);
            }
        }

        #[test]
        fn test_proxy_recv_timeout_invalid_handle() {
            let invalid_handle = FfiHandle { id: 99999 };
            let mut out_msg = std::mem::MaybeUninit::<FfiJsonRpcMessage>::uninit();
            let mut error: *mut FfiError = std::ptr::null_mut();

            let result =
                cvm_proxy_ch_recv_timeout(invalid_handle, 1, out_msg.as_mut_ptr(), &mut error);

            assert!(!result, "Should return false for invalid handle");
            assert!(!error.is_null(), "Should set error for invalid handle");

            unsafe {
                assert_eq!((*error).code, ErrorCode::Other);
                crate::types::cvm_error_free(error);
            }
        }
    }

    mod blocking_recv_tests {
        use super::*;

        #[test]
        fn test_server_recv_invalid_handle() {
            let invalid_handle = FfiHandle { id: 99999 };
            let mut out_req = std::mem::MaybeUninit::<FfiIncomingRequest>::uninit();
            let mut error: *mut FfiError = std::ptr::null_mut();

            let start = std::time::Instant::now();
            let result = cvm_server_ch_recv(invalid_handle, out_req.as_mut_ptr(), &mut error);
            let elapsed = start.elapsed();

            assert!(!result, "Should return false for invalid handle");
            assert!(!error.is_null(), "Should set error");
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "Should return quickly for invalid handle, took {:?}",
                elapsed
            );

            unsafe {
                assert_eq!((*error).code, ErrorCode::Other);
                crate::types::cvm_error_free(error);
            }
        }

        #[test]
        fn test_client_recv_invalid_handle() {
            let invalid_handle = FfiHandle { id: 99999 };
            let mut out_msg = std::mem::MaybeUninit::<FfiJsonRpcMessage>::uninit();
            let mut error: *mut FfiError = std::ptr::null_mut();

            let start = std::time::Instant::now();
            let result = cvm_client_ch_recv(invalid_handle, out_msg.as_mut_ptr(), &mut error);
            let elapsed = start.elapsed();

            assert!(!result, "Should return false for invalid handle");
            assert!(!error.is_null(), "Should set error");
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "Should return quickly for invalid handle, took {:?}",
                elapsed
            );

            unsafe {
                assert_eq!((*error).code, ErrorCode::Other);
                crate::types::cvm_error_free(error);
            }
        }

        #[test]
        fn test_gateway_recv_invalid_handle() {
            let invalid_handle = FfiHandle { id: 99999 };
            let mut out_req = std::mem::MaybeUninit::<FfiIncomingRequest>::uninit();
            let mut error: *mut FfiError = std::ptr::null_mut();

            let start = std::time::Instant::now();
            let result = cvm_gateway_ch_recv(invalid_handle, out_req.as_mut_ptr(), &mut error);
            let elapsed = start.elapsed();

            assert!(!result, "Should return false for invalid handle");
            assert!(!error.is_null(), "Should set error");
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "Should return quickly for invalid handle, took {:?}",
                elapsed
            );

            unsafe {
                assert_eq!((*error).code, ErrorCode::Other);
                crate::types::cvm_error_free(error);
            }
        }
    }

    mod error_handling_tests {
        use super::*;

        #[test]
        fn test_error_codes_match_c_header() {
            assert_eq!(ErrorCode::Ok as i32, 0, "CVM_OK");
            assert_eq!(ErrorCode::Transport as i32, 1, "CVM_TRANSPORT");
            assert_eq!(ErrorCode::Encryption as i32, 2, "CVM_ENCRYPTION");
            assert_eq!(ErrorCode::Decryption as i32, 3, "CVM_DECRYPTION");
            assert_eq!(ErrorCode::Timeout as i32, 4, "CVM_TIMEOUT");
            assert_eq!(ErrorCode::Validation as i32, 5, "CVM_VALIDATION");
            assert_eq!(ErrorCode::Unauthorized as i32, 6, "CVM_UNAUTHORIZED");
            assert_eq!(ErrorCode::Serialization as i32, 7, "CVM_SERIALIZATION");
            assert_eq!(ErrorCode::Other as i32, 99, "CVM_OTHER");
        }
    }
}
