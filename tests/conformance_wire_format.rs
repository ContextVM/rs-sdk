//! Conformance tests for ContextVM wire format: MCP JSON-RPC carried in Nostr kind 25910 events.
//!
//! These mirror the layering style of `src/rmcp_transport/pipeline_tests.rs`: build the JSON-RPC
//! payload, serialize through the same helpers the transport uses (`mcp_to_nostr_event`, tag
//! builders from [`BaseTransport`]), sign with nostr-sdk, then assert on kind, tags, and content.

use contextvm_sdk::core::constants::{
    mcp_protocol_version, tags, CTXVM_MESSAGES_KIND, INITIALIZE_METHOD,
    NOTIFICATIONS_INITIALIZED_METHOD, SERVER_ANNOUNCEMENT_KIND,
};
use contextvm_sdk::core::serializers;
use contextvm_sdk::core::types::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};
use contextvm_sdk::transport::base::BaseTransport;
use nostr_sdk::prelude::*;

fn assert_ctxvm_message_kind(event: &Event) {
    assert_eq!(
        event.kind,
        Kind::Custom(CTXVM_MESSAGES_KIND),
        "ContextVM MCP messages must use kind {}",
        CTXVM_MESSAGES_KIND
    );
}

fn p_tag_hex(event: &Event) -> Option<String> {
    serializers::get_tag_value(&event.tags, tags::PUBKEY)
}

fn e_tag_hex(event: &Event) -> Option<String> {
    serializers::get_tag_value(&event.tags, tags::EVENT_ID)
}

// ── Initialize request ───────────────────────────────────────────────────────

#[test]
fn ctxvm_initialize_request_has_kind_p_tag_and_jsonrpc_initialize() {
    let server_keys = Keys::generate();
    let server_pk = server_keys.public_key();

    let init_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(1),
        method: INITIALIZE_METHOD.to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": { "name": "conformance-test", "version": "0.0.0" }
        })),
    });

    let recipient_tags = BaseTransport::create_recipient_tags(&server_pk);
    let builder = serializers::mcp_to_nostr_event(&init_req, CTXVM_MESSAGES_KIND, recipient_tags)
        .expect("initialize request should serialize to event content");

    let client_keys = Keys::generate();
    let event = builder
        .sign_with_keys(&client_keys)
        .expect("sign initialize request event");

    assert_ctxvm_message_kind(&event);
    assert_eq!(
        p_tag_hex(&event),
        Some(server_pk.to_hex()),
        "initialize request must target the server via p tag"
    );

    let msg = serializers::nostr_event_to_mcp_message(&event.content)
        .expect("event content should be valid JSON-RPC");
    assert!(msg.is_request());
    assert_eq!(msg.method(), Some(INITIALIZE_METHOD));

    // Parse at the raw JSON level to verify wire format independently of the typed deserializer.
    let v: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON object");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], serde_json::json!(1));
}

// ── Initialize response ──────────────────────────────────────────────────────

#[test]
fn ctxvm_initialize_response_has_kind_e_tag_and_result_protocol_version() {
    let server_keys = Keys::generate();
    let server_pk = server_keys.public_key();
    let client_keys = Keys::generate();
    let client_pk = client_keys.public_key();

    // Signed request event provides the Nostr event id referenced by e on the response.
    let init_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-1"),
        method: INITIALIZE_METHOD.to_string(),
        params: Some(serde_json::json!({})),
    });
    let recipient_tags = BaseTransport::create_recipient_tags(&server_pk);
    let request_event =
        serializers::mcp_to_nostr_event(&init_req, CTXVM_MESSAGES_KIND, recipient_tags)
            .expect("request event for response correlation should serialize")
            .sign_with_keys(&client_keys)
            .expect("sign request event for correlation");

    let init_resp = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-1"),
        result: serde_json::json!({
            "protocolVersion": mcp_protocol_version(),
            "serverInfo": {
                "name": "conformance-test-server",
                "version": "0.0.0"
            },
            "capabilities": {}
        }),
    });

    let response_tags = BaseTransport::create_response_tags(&client_pk, &request_event.id);
    let response_event =
        serializers::mcp_to_nostr_event(&init_resp, CTXVM_MESSAGES_KIND, response_tags)
            .expect("initialize response should serialize")
            .sign_with_keys(&server_keys)
            .expect("sign initialize response event");

    assert_ctxvm_message_kind(&response_event);
    assert_eq!(
        p_tag_hex(&response_event),
        Some(client_pk.to_hex()),
        "initialize response must route back to the client via p tag"
    );
    assert_eq!(
        e_tag_hex(&response_event),
        Some(request_event.id.to_hex()),
        "initialize response must correlate to the request Nostr event via e tag"
    );

    let v: serde_json::Value =
        serde_json::from_str(&response_event.content).expect("content must be JSON");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], serde_json::json!("corr-1"));
    assert!(v["result"]["protocolVersion"].is_string());
    assert!(v["result"]["serverInfo"]["name"].is_string());
}

// ── notifications/initialized ──────────────────────────────────────────────

#[test]
fn ctxvm_notifications_initialized_has_kind_p_tag_and_method() {
    let server_keys = Keys::generate();
    let server_pk = server_keys.public_key();
    let client_keys = Keys::generate();

    let notif = JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: NOTIFICATIONS_INITIALIZED_METHOD.to_string(),
        params: None,
    });

    let recipient_tags = BaseTransport::create_recipient_tags(&server_pk);
    let event = serializers::mcp_to_nostr_event(&notif, CTXVM_MESSAGES_KIND, recipient_tags)
        .expect("notification should serialize")
        // Client sends this to the server; signer must differ from `p` so the tag is not stripped.
        .sign_with_keys(&client_keys)
        .expect("sign initialized notification");

    assert_ctxvm_message_kind(&event);
    assert_eq!(
        p_tag_hex(&event),
        Some(server_pk.to_hex()),
        "initialized notification must include server p tag"
    );

    let msg = serializers::nostr_event_to_mcp_message(&event.content).expect("parse content");
    assert!(msg.is_notification());
    assert_eq!(msg.method(), Some(NOTIFICATIONS_INITIALIZED_METHOD));

    // Parse at the raw JSON level to verify wire format independently of the typed deserializer.
    let v: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON object");
    assert_eq!(v["jsonrpc"], "2.0");
    assert!(
        v.get("id").is_none_or(serde_json::Value::is_null),
        "JSON-RPC notifications must not include an id"
    );
}

// ── tools/list request ───────────────────────────────────────────────────────

#[test]
fn ctxvm_tools_list_request_has_kind_p_tag_and_method() {
    let server_keys = Keys::generate();
    let server_pk = server_keys.public_key();

    let list_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(2),
        method: "tools/list".to_string(),
        params: Some(serde_json::json!({})),
    });

    let recipient_tags = BaseTransport::create_recipient_tags(&server_pk);
    let builder = serializers::mcp_to_nostr_event(&list_req, CTXVM_MESSAGES_KIND, recipient_tags)
        .expect("tools/list request should serialize to event content");

    let client_keys = Keys::generate();
    let event = builder
        .sign_with_keys(&client_keys)
        .expect("sign tools/list request event");

    assert_ctxvm_message_kind(&event);
    assert_eq!(
        p_tag_hex(&event),
        Some(server_pk.to_hex()),
        "tools/list request must target the server via p tag"
    );

    let msg = serializers::nostr_event_to_mcp_message(&event.content)
        .expect("event content should be valid JSON-RPC");
    assert!(msg.is_request());
    assert_eq!(msg.method(), Some("tools/list"));

    let v: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON object");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], serde_json::json!(2));
}

// ── tools/call request ───────────────────────────────────────────────────────

#[test]
fn ctxvm_tools_call_request_has_kind_p_tag_method_and_params() {
    let server_keys = Keys::generate();
    let server_pk = server_keys.public_key();

    let call_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(3),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "add",
            "arguments": { "a": 5, "b": 3 }
        })),
    });

    let recipient_tags = BaseTransport::create_recipient_tags(&server_pk);
    let builder = serializers::mcp_to_nostr_event(&call_req, CTXVM_MESSAGES_KIND, recipient_tags)
        .expect("tools/call request should serialize to event content");

    let client_keys = Keys::generate();
    let event = builder
        .sign_with_keys(&client_keys)
        .expect("sign tools/call request event");

    assert_ctxvm_message_kind(&event);
    assert_eq!(
        p_tag_hex(&event),
        Some(server_pk.to_hex()),
        "tools/call request must target the server via p tag"
    );

    let msg = serializers::nostr_event_to_mcp_message(&event.content)
        .expect("event content should be valid JSON-RPC");
    assert!(msg.is_request());
    assert_eq!(msg.method(), Some("tools/call"));

    let v: serde_json::Value =
        serde_json::from_str(&event.content).expect("content must be JSON object");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], serde_json::json!(3));
    assert_eq!(v["params"]["name"], "add");
    assert!(
        v["params"]["arguments"].is_object(),
        "tools/call params.arguments must be an object on the wire"
    );
}

// ── tools/list response ───────────────────────────────────────────────────────

#[test]
fn ctxvm_tools_list_response_has_kind_e_tag_and_result() {
    let server_keys = Keys::generate();
    let server_pk = server_keys.public_key();
    let client_keys = Keys::generate();
    let client_pk = client_keys.public_key();

    let list_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-tools-list"),
        method: "tools/list".to_string(),
        params: Some(serde_json::json!({})),
    });
    let recipient_tags = BaseTransport::create_recipient_tags(&server_pk);
    let request_event =
        serializers::mcp_to_nostr_event(&list_req, CTXVM_MESSAGES_KIND, recipient_tags)
            .expect("tools/list request for response correlation should serialize")
            .sign_with_keys(&client_keys)
            .expect("sign tools/list request event for correlation");

    let list_resp = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-tools-list"),
        result: serde_json::json!({ "tools": [] }),
    });

    let response_tags = BaseTransport::create_response_tags(&client_pk, &request_event.id);
    let response_event =
        serializers::mcp_to_nostr_event(&list_resp, CTXVM_MESSAGES_KIND, response_tags)
            .expect("tools/list response should serialize")
            .sign_with_keys(&server_keys)
            .expect("sign tools/list response event");

    assert_ctxvm_message_kind(&response_event);
    assert_eq!(
        p_tag_hex(&response_event),
        Some(client_pk.to_hex()),
        "tools/list response must route back to the client via p tag"
    );
    assert_eq!(
        e_tag_hex(&response_event),
        Some(request_event.id.to_hex()),
        "tools/list response must correlate to the request Nostr event via e tag"
    );

    let v: serde_json::Value =
        serde_json::from_str(&response_event.content).expect("content must be JSON");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], serde_json::json!("corr-tools-list"));
    assert!(v["result"]["tools"].is_array());
}

// ── tools/call response ───────────────────────────────────────────────────────

#[test]
fn ctxvm_tools_call_response_has_kind_e_tag_and_result() {
    let server_keys = Keys::generate();
    let server_pk = server_keys.public_key();
    let client_keys = Keys::generate();
    let client_pk = client_keys.public_key();

    let call_req = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-tools-call"),
        method: "tools/call".to_string(),
        params: Some(serde_json::json!({
            "name": "add",
            "arguments": { "a": 5, "b": 3 }
        })),
    });
    let recipient_tags = BaseTransport::create_recipient_tags(&server_pk);
    let request_event =
        serializers::mcp_to_nostr_event(&call_req, CTXVM_MESSAGES_KIND, recipient_tags)
            .expect("tools/call request for response correlation should serialize")
            .sign_with_keys(&client_keys)
            .expect("sign tools/call request event for correlation");

    let call_resp = JsonRpcMessage::Response(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!("corr-tools-call"),
        result: serde_json::json!({
            "content": [{ "type": "text", "text": "8" }],
            "isError": false
        }),
    });

    let response_tags = BaseTransport::create_response_tags(&client_pk, &request_event.id);
    let response_event =
        serializers::mcp_to_nostr_event(&call_resp, CTXVM_MESSAGES_KIND, response_tags)
            .expect("tools/call response should serialize")
            .sign_with_keys(&server_keys)
            .expect("sign tools/call response event");

    assert_ctxvm_message_kind(&response_event);
    assert_eq!(
        p_tag_hex(&response_event),
        Some(client_pk.to_hex()),
        "tools/call response must route back to the client via p tag"
    );
    assert_eq!(
        e_tag_hex(&response_event),
        Some(request_event.id.to_hex()),
        "tools/call response must correlate to the request Nostr event via e tag"
    );

    let v: serde_json::Value =
        serde_json::from_str(&response_event.content).expect("content must be JSON");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], serde_json::json!("corr-tools-call"));
    assert!(v["result"]["content"].is_array());
    assert_eq!(v["result"]["isError"], serde_json::json!(false));
}

// ── Server announcement (kind 11316) ──────────────────────────────────────────

#[test]
fn ctxvm_server_announcement_has_kind_and_required_tags() {
    let server_keys = Keys::generate();

    // MCP-flavoured JSON for wire conformance; not the same content shape as `NostrServerTransport::announce` (flat `ServerInfo` only).
    let content = serde_json::json!({
        "protocolVersion": mcp_protocol_version(),
        "serverInfo": { "name": "Test Server" },
        "capabilities": {},
    });
    let content_str = serde_json::to_string(&content).expect("announcement content must serialize");

    let announcement_tags = vec![
        Tag::custom(
            TagKind::Custom(tags::NAME.into()),
            vec!["Test Server".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(tags::ABOUT.into()),
            vec!["A test server".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(tags::WEBSITE.into()),
            vec!["http://localhost".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(tags::SUPPORT_ENCRYPTION.into()),
            Vec::<String>::new(),
        ),
    ];

    let event = EventBuilder::new(Kind::Custom(SERVER_ANNOUNCEMENT_KIND), content_str)
        .tags(announcement_tags)
        .sign_with_keys(&server_keys)
        .expect("sign server announcement event");

    assert_eq!(
        event.kind,
        Kind::Custom(SERVER_ANNOUNCEMENT_KIND),
        "server announcement must use kind {}",
        SERVER_ANNOUNCEMENT_KIND
    );
    assert_eq!(event.pubkey, server_keys.public_key());

    assert_eq!(
        serializers::get_tag_value(&event.tags, tags::NAME).as_deref(),
        Some("Test Server")
    );
    assert_eq!(
        serializers::get_tag_value(&event.tags, tags::ABOUT).as_deref(),
        Some("A test server")
    );
    assert_eq!(
        serializers::get_tag_value(&event.tags, tags::WEBSITE).as_deref(),
        Some("http://localhost")
    );

    assert!(
        event.tags.iter().any(|t| {
            let parts = t.clone().to_vec();
            parts.len() == 1 && parts.first().map(|s| s.as_str()) == Some(tags::SUPPORT_ENCRYPTION)
        }),
        "support_encryption must be present as a single-element tag"
    );

    let v: serde_json::Value =
        serde_json::from_str(&event.content).expect("announcement content must be JSON");
    assert_eq!(v["protocolVersion"], mcp_protocol_version());
    assert_eq!(v["serverInfo"]["name"], "Test Server");
    assert!(v["capabilities"].is_object());
}
