//! UniFFI-compatible types and high-level object-oriented API.
//!
//! These types are exposed via UniFFI proc-macros and provide a more ergonomic
//! interface than the flat C API.  They are designed for Python, Swift, and
//! Kotlin consumers.

use crate::builders::{
    build_sdk_client_config_from_fields, build_sdk_server_config_from_fields,
    CapabilityExclusionParts, ClientConfigParts, ServerConfigParts,
};
use crate::error::FfiError;
use crate::runtime::global_runtime;
use crate::types::json_rpc_id_to_string;
use std::sync::Arc;
use std::time::Duration;

// ─── Enum mirrors for UniFFI ───────────────────────────────────────────

/// Encryption mode.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum EncryptionMode {
    Optional,
    Required,
    Disabled,
}

/// Gift-wrap mode (CEP-19).
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum GiftWrapMode {
    Optional,
    Ephemeral,
    Persistent,
}

/// JSON-RPC message type.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum JsonRpcMessageType {
    Request,
    Response,
    ErrorResponse,
    Notification,
}

// ─── Record types for UniFFI ───────────────────────────────────────────

/// A JSON-RPC message.
#[derive(Debug, Clone, uniffi::Record)]
pub struct JsonRpcMessage {
    pub msg_type: JsonRpcMessageType,
    pub payload_json: String,
    pub method: String,
    pub id: String,
}

/// An incoming MCP request (server-side).
#[derive(Debug, Clone, uniffi::Record)]
pub struct IncomingRequest {
    pub message: JsonRpcMessage,
    pub client_pubkey: String,
    pub event_id: String,
    pub is_encrypted: bool,
}

/// A discovered server announcement.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ServerAnnouncement {
    pub pubkey: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub picture: Option<String>,
    pub about: Option<String>,
    pub website: Option<String>,
    pub event_id: String,
}

/// Nostr profile metadata for a provider.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ProviderProfile {
    pub pubkey: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub nip05: Option<String>,
}

/// A capability exclusion pattern that bypasses pubkey whitelisting.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CapabilityExclusion {
    pub method: String,
    pub name: Option<String>,
}

/// Learned peer capability flags.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct PeerCapabilities {
    pub supports_encryption: bool,
    pub supports_ephemeral_encryption: bool,
    pub supports_oversized_transfer: bool,
}

/// A discovered MCP tool and provider metadata used by foreign clients.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DiscoveredTool {
    pub provider_pubkey: String,
    pub provider_display_name: Option<String>,
    pub provider_name: Option<String>,
    pub provider_about: Option<String>,
    pub provider_picture: Option<String>,
    pub provider_nip05: Option<String>,
    pub tool_name: String,
    pub description: String,
    pub schema_json: String,
}

/// Server transport configuration.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ServerConfig {
    pub relay_urls: Vec<String>,
    pub encryption_mode: EncryptionMode,
    pub gift_wrap_mode: GiftWrapMode,
    pub is_announced_server: bool,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub server_picture: Option<String>,
    pub server_about: Option<String>,
    pub server_website: Option<String>,
    pub allowed_pubkeys: Vec<String>,
    pub session_timeout_secs: u64,
    pub cleanup_interval_secs: u64,
    pub excluded_capabilities: Vec<CapabilityExclusion>,
    pub max_sessions: u64,
    pub request_timeout_secs: u64,
    pub relay_list_urls: Vec<String>,
    pub bootstrap_relay_urls: Vec<String>,
    pub publish_relay_list: bool,
    pub profile_metadata_json: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec!["wss://relay.damus.io".to_string()],
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            is_announced_server: false,
            server_name: None,
            server_version: None,
            server_picture: None,
            server_about: None,
            server_website: None,
            allowed_pubkeys: vec![],
            session_timeout_secs: 300,
            cleanup_interval_secs: 60,
            excluded_capabilities: vec![],
            max_sessions: 1000,
            request_timeout_secs: 60,
            relay_list_urls: vec![],
            bootstrap_relay_urls: vec![],
            publish_relay_list: true,
            profile_metadata_json: None,
        }
    }
}

/// Client transport configuration.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ClientConfig {
    pub relay_urls: Vec<String>,
    pub server_pubkey: String,
    pub encryption_mode: EncryptionMode,
    pub gift_wrap_mode: GiftWrapMode,
    pub is_stateless: bool,
    pub timeout_secs: u64,
    pub discovery_relay_urls: Vec<String>,
    pub fallback_operational_relay_urls: Vec<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            relay_urls: vec![],
            server_pubkey: String::new(),
            encryption_mode: EncryptionMode::Optional,
            gift_wrap_mode: GiftWrapMode::Optional,
            is_stateless: false,
            timeout_secs: 30,
            discovery_relay_urls: vec![],
            fallback_operational_relay_urls: vec![],
        }
    }
}

// ─── Conversion helpers ────────────────────────────────────────────────

fn sdk_encryption_mode(m: EncryptionMode) -> contextvm_sdk::EncryptionMode {
    match m {
        EncryptionMode::Optional => contextvm_sdk::EncryptionMode::Optional,
        EncryptionMode::Required => contextvm_sdk::EncryptionMode::Required,
        EncryptionMode::Disabled => contextvm_sdk::EncryptionMode::Disabled,
    }
}

fn sdk_gift_wrap_mode(m: GiftWrapMode) -> contextvm_sdk::GiftWrapMode {
    match m {
        GiftWrapMode::Optional => contextvm_sdk::GiftWrapMode::Optional,
        GiftWrapMode::Ephemeral => contextvm_sdk::GiftWrapMode::Ephemeral,
        GiftWrapMode::Persistent => contextvm_sdk::GiftWrapMode::Persistent,
    }
}

fn message_to_uniffi(msg: &contextvm_sdk::JsonRpcMessage) -> JsonRpcMessage {
    let msg_type = match msg {
        contextvm_sdk::JsonRpcMessage::Request(_) => JsonRpcMessageType::Request,
        contextvm_sdk::JsonRpcMessage::Response(_) => JsonRpcMessageType::Response,
        contextvm_sdk::JsonRpcMessage::ErrorResponse(_) => JsonRpcMessageType::ErrorResponse,
        contextvm_sdk::JsonRpcMessage::Notification(_) => JsonRpcMessageType::Notification,
    };

    JsonRpcMessage {
        msg_type,
        payload_json: serde_json::to_string(msg).unwrap_or_default(),
        method: msg.method().map(String::from).unwrap_or_default(),
        id: msg.id().map(json_rpc_id_to_string).unwrap_or_default(),
    }
}

fn incoming_to_uniffi(req: &contextvm_sdk::IncomingRequest) -> IncomingRequest {
    IncomingRequest {
        message: message_to_uniffi(&req.message),
        client_pubkey: req.client_pubkey.clone(),
        event_id: req.event_id.clone(),
        is_encrypted: req.is_encrypted,
    }
}

fn parse_json_rpc(json: &str) -> Result<contextvm_sdk::JsonRpcMessage, FfiError> {
    serde_json::from_str(json).map_err(|e| FfiError {
        code: crate::error::ErrorCode::Serialization,
        message: e.to_string(),
    })
}

type IncomingRx =
    Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::IncomingRequest>>>;
type MessageRx =
    Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::JsonRpcMessage>>>;

fn channel_closed() -> FfiError {
    FfiError {
        code: crate::error::ErrorCode::Transport,
        message: "channel closed".into(),
    }
}

fn recv_timeout_error() -> FfiError {
    FfiError {
        code: crate::error::ErrorCode::Timeout,
        message: "receive timed out".into(),
    }
}

fn recv_incoming(rx: IncomingRx) -> Result<IncomingRequest, FfiError> {
    global_runtime()
        .block_on(async {
            let mut guard = rx.lock().await;
            guard.recv().await
        })
        .map(|req| incoming_to_uniffi(&req))
        .ok_or_else(channel_closed)
}

fn recv_incoming_timeout(rx: IncomingRx, timeout_secs: u64) -> Result<IncomingRequest, FfiError> {
    match global_runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            let mut guard = rx.lock().await;
            guard.recv().await
        })
        .await
    }) {
        Ok(Some(req)) => Ok(incoming_to_uniffi(&req)),
        Ok(None) => Err(channel_closed()),
        Err(_) => Err(recv_timeout_error()),
    }
}

fn recv_incoming_try(rx: IncomingRx) -> Result<Option<IncomingRequest>, FfiError> {
    let mut guard = match rx.try_lock() {
        Ok(guard) => guard,
        Err(_) => return Ok(None),
    };
    match guard.try_recv() {
        Ok(req) => Ok(Some(incoming_to_uniffi(&req))),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => Err(channel_closed()),
    }
}

fn recv_message(rx: MessageRx) -> Result<JsonRpcMessage, FfiError> {
    global_runtime()
        .block_on(async {
            let mut guard = rx.lock().await;
            guard.recv().await
        })
        .map(|msg| message_to_uniffi(&msg))
        .ok_or_else(channel_closed)
}

fn recv_message_timeout(rx: MessageRx, timeout_secs: u64) -> Result<JsonRpcMessage, FfiError> {
    match global_runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(timeout_secs), async {
            let mut guard = rx.lock().await;
            guard.recv().await
        })
        .await
    }) {
        Ok(Some(msg)) => Ok(message_to_uniffi(&msg)),
        Ok(None) => Err(channel_closed()),
        Err(_) => Err(recv_timeout_error()),
    }
}

fn recv_message_try(rx: MessageRx) -> Result<Option<JsonRpcMessage>, FfiError> {
    let mut guard = match rx.try_lock() {
        Ok(guard) => guard,
        Err(_) => return Ok(None),
    };
    match guard.try_recv() {
        Ok(msg) => Ok(Some(message_to_uniffi(&msg))),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => Err(channel_closed()),
    }
}

fn tool_to_uniffi(tool: crate::discovery::DiscoveredToolRecord) -> DiscoveredTool {
    DiscoveredTool {
        provider_pubkey: tool.provider_pubkey,
        provider_display_name: tool.provider_display_name,
        provider_name: tool.provider_name,
        provider_about: tool.provider_about,
        provider_picture: tool.provider_picture,
        provider_nip05: tool.provider_nip05,
        tool_name: tool.tool_name,
        description: tool.description,
        schema_json: tool.schema_json,
    }
}

fn profile_to_uniffi(profile: crate::discovery::ProviderProfileRecord) -> ProviderProfile {
    ProviderProfile {
        pubkey: profile.pubkey,
        name: profile.name,
        about: profile.about,
        picture: profile.picture,
        nip05: profile.nip05,
    }
}

fn capabilities_to_uniffi(caps: contextvm_sdk::PeerCapabilities) -> PeerCapabilities {
    PeerCapabilities {
        supports_encryption: caps.supports_encryption,
        supports_ephemeral_encryption: caps.supports_ephemeral_encryption,
        supports_oversized_transfer: caps.supports_oversized_transfer,
    }
}

fn parse_json_value_array(json: &str, name: &str) -> Result<Vec<serde_json::Value>, FfiError> {
    serde_json::from_str(json).map_err(|e| FfiError {
        code: crate::error::ErrorCode::Serialization,
        message: format!("invalid {name}: {e}"),
    })
}

fn parse_tags_json(json: &str) -> Result<Vec<nostr_sdk::prelude::Tag>, FfiError> {
    let parts: Vec<Vec<String>> = serde_json::from_str(json).map_err(|e| FfiError {
        code: crate::error::ErrorCode::Serialization,
        message: format!("invalid tags_json: {e}"),
    })?;
    parts
        .into_iter()
        .map(|tag| {
            nostr_sdk::prelude::Tag::parse(tag).map_err(|e| FfiError {
                code: crate::error::ErrorCode::Validation,
                message: e.to_string(),
            })
        })
        .collect()
}

// ─── High-level UniFFI objects ─────────────────────────────────────────

/// A Nostr keypair.
#[derive(uniffi::Object)]
pub struct Keys {
    inner: contextvm_sdk::signer::Keys,
}

#[uniffi::export]
impl Keys {
    /// Generate a new random keypair.
    #[uniffi::constructor]
    pub fn generate() -> Self {
        Self {
            inner: contextvm_sdk::signer::generate(),
        }
    }

    /// Create keys from a secret key (hex or nsec/bech32).
    #[uniffi::constructor]
    pub fn from_secret_key(sk: &str) -> Result<Self, FfiError> {
        contextvm_sdk::signer::from_sk(sk)
            .map(|inner| Self { inner })
            .map_err(|e| FfiError {
                code: crate::error::ErrorCode::Other,
                message: e.to_string(),
            })
    }

    /// Get the public key (hex).
    pub fn public_key(&self) -> String {
        self.inner.public_key().to_hex()
    }

    /// Get the secret key (hex).
    pub fn secret_key(&self) -> String {
        self.inner.secret_key().to_secret_hex()
    }
}

/// A relay pool for Nostr connectivity.
#[derive(uniffi::Object)]
pub struct RelayPool {
    inner: contextvm_sdk::RelayPool,
}

#[uniffi::export]
impl RelayPool {
    /// Create a new relay pool.
    #[uniffi::constructor]
    pub fn new(keys: &Keys) -> Result<Self, FfiError> {
        global_runtime()
            .block_on(contextvm_sdk::RelayPool::new(keys.inner.clone()))
            .map(|inner| Self { inner })
            .map_err(FfiError::from)
    }

    /// Connect to relays.
    pub fn connect(&self, relay_urls: Vec<String>) -> Result<(), FfiError> {
        global_runtime()
            .block_on(self.inner.connect(&relay_urls))
            .map_err(FfiError::from)
    }

    /// Disconnect from all relays.
    pub fn disconnect(&self) -> Result<(), FfiError> {
        global_runtime()
            .block_on(self.inner.disconnect())
            .map_err(FfiError::from)
    }
}

/// A server transport that receives MCP requests over Nostr.
#[derive(uniffi::Object)]
pub struct Server {
    transport: Arc<tokio::sync::Mutex<contextvm_sdk::NostrServerTransport>>,
    receiver: Arc<
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::IncomingRequest>>,
    >,
}

#[uniffi::export]
impl Server {
    /// Create and start a server transport.
    #[uniffi::constructor]
    pub fn new(keys: &Keys, config: &ServerConfig) -> Result<Self, FfiError> {
        let sdk_config = build_sdk_server_config_from_fields(ServerConfigParts {
            relay_urls: config.relay_urls.clone(),
            encryption_mode: sdk_encryption_mode(config.encryption_mode),
            gift_wrap_mode: sdk_gift_wrap_mode(config.gift_wrap_mode),
            server_name: config.server_name.clone(),
            server_version: config.server_version.clone(),
            server_picture: config.server_picture.clone(),
            server_about: config.server_about.clone(),
            server_website: config.server_website.clone(),
            is_announced_server: config.is_announced_server,
            allowed_pubkeys: config.allowed_pubkeys.clone(),
            session_timeout_secs: config.session_timeout_secs,
            cleanup_interval_secs: config.cleanup_interval_secs,
            excluded_capabilities: config
                .excluded_capabilities
                .iter()
                .map(|cap| CapabilityExclusionParts {
                    method: cap.method.clone(),
                    name: cap.name.clone(),
                })
                .collect(),
            max_sessions: config.max_sessions as usize,
            request_timeout_secs: config.request_timeout_secs,
            relay_list_urls: config.relay_list_urls.clone(),
            bootstrap_relay_urls: config.bootstrap_relay_urls.clone(),
            publish_relay_list: config.publish_relay_list,
            profile_metadata_json: config.profile_metadata_json.clone(),
        })?;

        global_runtime()
            .block_on(async {
                let mut transport =
                    contextvm_sdk::NostrServerTransport::new(keys.inner.clone(), sdk_config)
                        .await?;
                transport.start().await?;
                transport.spawn_discoverability_publication();
                let receiver = transport
                    .take_message_receiver()
                    .ok_or_else(|| contextvm_sdk::Error::Other("receiver already taken".into()))?;
                Ok::<_, contextvm_sdk::Error>(Self {
                    transport: Arc::new(tokio::sync::Mutex::new(transport)),
                    receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
                })
            })
            .map_err(FfiError::from)
    }

    /// Receive the next incoming request.  Blocks until one is available.
    pub fn recv(&self) -> Result<IncomingRequest, FfiError> {
        recv_incoming(self.receiver.clone())
    }

    /// Receive the next incoming request, timing out after `timeout_secs`.
    pub fn recv_timeout(&self, timeout_secs: u64) -> Result<IncomingRequest, FfiError> {
        recv_incoming_timeout(self.receiver.clone(), timeout_secs)
    }

    /// Return the next incoming request if one is already buffered.
    pub fn recv_try(&self) -> Result<Option<IncomingRequest>, FfiError> {
        recv_incoming_try(self.receiver.clone())
    }

    /// Send a response for a given event ID.
    pub fn send_response(&self, event_id: &str, payload_json: &str) -> Result<(), FfiError> {
        let message = parse_json_rpc(payload_json)?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.send_response(event_id, message).await
            })
            .map_err(FfiError::from)
    }

    /// Send a notification to a specific client.
    pub fn send_notification(
        &self,
        client_pubkey: &str,
        payload_json: &str,
        correlated_event_id: Option<String>,
    ) -> Result<(), FfiError> {
        let message = parse_json_rpc(payload_json)?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard
                    .send_notification(client_pubkey, &message, correlated_event_id.as_deref())
                    .await
            })
            .map_err(FfiError::from)
    }

    /// Broadcast a notification to all initialized clients.
    pub fn broadcast_notification(&self, payload_json: &str) -> Result<(), FfiError> {
        let message = parse_json_rpc(payload_json)?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.broadcast_notification(&message).await
            })
            .map_err(FfiError::from)
    }

    /// Sets extra announcement/discovery tags from a JSON array of tag arrays.
    pub fn set_announcement_extra_tags(&self, tags_json: &str) -> Result<(), FfiError> {
        let tags = parse_tags_json(tags_json)?;
        let transport = self.transport.clone();
        global_runtime().block_on(async {
            let mut guard = transport.lock().await;
            guard.set_announcement_extra_tags(tags);
        });
        Ok(())
    }

    /// Sets pricing tags from a JSON array of tag arrays.
    pub fn set_announcement_pricing_tags(&self, tags_json: &str) -> Result<(), FfiError> {
        let tags = parse_tags_json(tags_json)?;
        let transport = self.transport.clone();
        global_runtime().block_on(async {
            let mut guard = transport.lock().await;
            guard.set_announcement_pricing_tags(tags);
        });
        Ok(())
    }

    /// Publish server announcement.
    pub fn announce(&self) -> Result<(), FfiError> {
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.announce().await
            })
            .map(|_| ())
            .map_err(FfiError::from)
    }

    /// Publish server announcement and return the Nostr event ID.
    pub fn announce_event_id(&self) -> Result<String, FfiError> {
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.announce().await
            })
            .map(|event_id| event_id.to_hex())
            .map_err(FfiError::from)
    }

    /// Publish tools list and return the Nostr event ID.
    pub fn publish_tools(&self, tools_json: &str) -> Result<String, FfiError> {
        let tools = parse_json_value_array(tools_json, "tools_json")?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.publish_tools(tools).await
            })
            .map(|event_id| event_id.to_hex())
            .map_err(FfiError::from)
    }

    /// Publish resources list and return the Nostr event ID.
    pub fn publish_resources(&self, resources_json: &str) -> Result<String, FfiError> {
        let resources = parse_json_value_array(resources_json, "resources_json")?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.publish_resources(resources).await
            })
            .map(|event_id| event_id.to_hex())
            .map_err(FfiError::from)
    }

    /// Publish prompts list and return the Nostr event ID.
    pub fn publish_prompts(&self, prompts_json: &str) -> Result<String, FfiError> {
        let prompts = parse_json_value_array(prompts_json, "prompts_json")?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.publish_prompts(prompts).await
            })
            .map(|event_id| event_id.to_hex())
            .map_err(FfiError::from)
    }

    /// Publish resource templates list and return the Nostr event ID.
    pub fn publish_resource_templates(&self, templates_json: &str) -> Result<String, FfiError> {
        let templates = parse_json_value_array(templates_json, "templates_json")?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.publish_resource_templates(templates).await
            })
            .map(|event_id| event_id.to_hex())
            .map_err(FfiError::from)
    }

    /// Delete previously published server announcements.
    pub fn delete_announcements(&self, reason: &str) -> Result<(), FfiError> {
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.delete_announcements(reason).await
            })
            .map_err(FfiError::from)
    }

    /// Close the server transport.
    pub fn close(&self) -> Result<(), FfiError> {
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let mut guard = transport.lock().await;
                guard.close().await
            })
            .map_err(FfiError::from)
    }
}

/// A client transport that sends MCP requests over Nostr.
#[derive(uniffi::Object)]
pub struct Client {
    transport: Arc<tokio::sync::Mutex<contextvm_sdk::NostrClientTransport>>,
    receiver: Arc<
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::JsonRpcMessage>>,
    >,
}

#[uniffi::export]
impl Client {
    /// Create and start a client transport.
    #[uniffi::constructor]
    pub fn new(keys: &Keys, config: &ClientConfig) -> Result<Self, FfiError> {
        let sdk_config = build_sdk_client_config_from_fields(ClientConfigParts {
            relay_urls: config.relay_urls.clone(),
            server_pubkey: config.server_pubkey.clone(),
            encryption_mode: sdk_encryption_mode(config.encryption_mode),
            gift_wrap_mode: sdk_gift_wrap_mode(config.gift_wrap_mode),
            is_stateless: config.is_stateless,
            timeout_secs: config.timeout_secs,
            discovery_relay_urls: config.discovery_relay_urls.clone(),
            fallback_operational_relay_urls: config.fallback_operational_relay_urls.clone(),
        });

        global_runtime()
            .block_on(async {
                let mut transport =
                    contextvm_sdk::NostrClientTransport::new(keys.inner.clone(), sdk_config)
                        .await?;
                transport.start().await?;
                let receiver = transport
                    .take_message_receiver()
                    .ok_or_else(|| contextvm_sdk::Error::Other("receiver already taken".into()))?;
                Ok::<_, contextvm_sdk::Error>(Self {
                    transport: Arc::new(tokio::sync::Mutex::new(transport)),
                    receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
                })
            })
            .map_err(FfiError::from)
    }

    /// Send a JSON-RPC message.
    pub fn send(&self, payload_json: &str) -> Result<(), FfiError> {
        let message = parse_json_rpc(payload_json)?;
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.send(&message).await
            })
            .map_err(FfiError::from)
    }

    /// Receive the next response.  Blocks until one is available.
    pub fn recv(&self) -> Result<JsonRpcMessage, FfiError> {
        recv_message(self.receiver.clone())
    }

    /// Receive the next response, timing out after `timeout_secs`.
    pub fn recv_timeout(&self, timeout_secs: u64) -> Result<JsonRpcMessage, FfiError> {
        recv_message_timeout(self.receiver.clone(), timeout_secs)
    }

    /// Return the next response if one is already buffered.
    pub fn recv_try(&self) -> Result<Option<JsonRpcMessage>, FfiError> {
        recv_message_try(self.receiver.clone())
    }

    /// Return a snapshot of server capabilities learned from discovery tags.
    pub fn discovered_server_capabilities(&self) -> PeerCapabilities {
        let transport = self.transport.clone();
        let caps = global_runtime().block_on(async {
            let guard = transport.lock().await;
            guard.discovered_server_capabilities()
        });
        capabilities_to_uniffi(caps)
    }

    /// Return whether the client has learned ephemeral gift-wrap support.
    pub fn server_supports_ephemeral_encryption(&self) -> bool {
        let transport = self.transport.clone();
        global_runtime().block_on(async {
            let guard = transport.lock().await;
            guard.server_supports_ephemeral_encryption()
        })
    }

    /// Return the first server event carrying discovery tags as JSON, if present.
    pub fn server_initialize_event_json(&self) -> Result<Option<String>, FfiError> {
        let transport = self.transport.clone();
        let event = global_runtime().block_on(async {
            let guard = transport.lock().await;
            guard.get_server_initialize_event()
        });
        event
            .map(|event| {
                serde_json::to_string(&event).map_err(|e| FfiError {
                    code: crate::error::ErrorCode::Serialization,
                    message: e.to_string(),
                })
            })
            .transpose()
    }

    /// Close the client transport.
    pub fn close(&self) -> Result<(), FfiError> {
        let transport = self.transport.clone();
        global_runtime()
            .block_on(async {
                let mut guard = transport.lock().await;
                guard.close().await
            })
            .map_err(FfiError::from)
    }
}

/// A gateway that bridges a local MCP server to Nostr.
#[derive(uniffi::Object)]
pub struct Gateway {
    gateway: Arc<tokio::sync::Mutex<contextvm_sdk::gateway::NostrMCPGateway>>,
    receiver: Arc<
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::IncomingRequest>>,
    >,
}

#[uniffi::export]
impl Gateway {
    /// Create and start a gateway transport.
    #[uniffi::constructor]
    pub fn new(keys: &Keys, config: &ServerConfig) -> Result<Self, FfiError> {
        let sdk_config = build_sdk_server_config_from_fields(ServerConfigParts {
            relay_urls: config.relay_urls.clone(),
            encryption_mode: sdk_encryption_mode(config.encryption_mode),
            gift_wrap_mode: sdk_gift_wrap_mode(config.gift_wrap_mode),
            server_name: config.server_name.clone(),
            server_version: config.server_version.clone(),
            server_picture: config.server_picture.clone(),
            server_about: config.server_about.clone(),
            server_website: config.server_website.clone(),
            is_announced_server: config.is_announced_server,
            allowed_pubkeys: config.allowed_pubkeys.clone(),
            session_timeout_secs: config.session_timeout_secs,
            cleanup_interval_secs: config.cleanup_interval_secs,
            excluded_capabilities: config
                .excluded_capabilities
                .iter()
                .map(|cap| CapabilityExclusionParts {
                    method: cap.method.clone(),
                    name: cap.name.clone(),
                })
                .collect(),
            max_sessions: config.max_sessions as usize,
            request_timeout_secs: config.request_timeout_secs,
            relay_list_urls: config.relay_list_urls.clone(),
            bootstrap_relay_urls: config.bootstrap_relay_urls.clone(),
            publish_relay_list: config.publish_relay_list,
            profile_metadata_json: config.profile_metadata_json.clone(),
        })?;
        let gateway_config = contextvm_sdk::gateway::GatewayConfig::new(sdk_config);

        global_runtime()
            .block_on(async {
                let mut gateway = contextvm_sdk::gateway::NostrMCPGateway::new(
                    keys.inner.clone(),
                    gateway_config,
                )
                .await?;
                let receiver = gateway.start().await?;
                Ok::<_, contextvm_sdk::Error>(Self {
                    gateway: Arc::new(tokio::sync::Mutex::new(gateway)),
                    receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
                })
            })
            .map_err(FfiError::from)
    }

    /// Receive the next incoming request.
    pub fn recv(&self) -> Result<IncomingRequest, FfiError> {
        recv_incoming(self.receiver.clone())
    }

    /// Receive the next incoming request, timing out after `timeout_secs`.
    pub fn recv_timeout(&self, timeout_secs: u64) -> Result<IncomingRequest, FfiError> {
        recv_incoming_timeout(self.receiver.clone(), timeout_secs)
    }

    /// Return the next incoming request if one is already buffered.
    pub fn recv_try(&self) -> Result<Option<IncomingRequest>, FfiError> {
        recv_incoming_try(self.receiver.clone())
    }

    /// Send a response for a given event ID.
    pub fn send_response(&self, event_id: &str, payload_json: &str) -> Result<(), FfiError> {
        let message = parse_json_rpc(payload_json)?;
        let gateway = self.gateway.clone();
        global_runtime()
            .block_on(async {
                let guard = gateway.lock().await;
                guard.send_response(event_id, message).await
            })
            .map_err(FfiError::from)
    }

    /// Publish server announcement.
    pub fn announce(&self) -> Result<(), FfiError> {
        let gateway = self.gateway.clone();
        global_runtime()
            .block_on(async {
                let guard = gateway.lock().await;
                guard.announce().await
            })
            .map(|_| ())
            .map_err(FfiError::from)
    }

    /// Publish server announcement and return the Nostr event ID.
    pub fn announce_event_id(&self) -> Result<String, FfiError> {
        let gateway = self.gateway.clone();
        global_runtime()
            .block_on(async {
                let guard = gateway.lock().await;
                guard.announce().await
            })
            .map(|event_id| event_id.to_hex())
            .map_err(FfiError::from)
    }

    /// Check if the gateway is active.
    pub fn is_active(&self) -> bool {
        let gateway = self.gateway.clone();
        global_runtime().block_on(async {
            let guard = gateway.lock().await;
            guard.is_active()
        })
    }

    /// Stop the gateway transport.
    pub fn stop(&self) -> Result<(), FfiError> {
        let gateway = self.gateway.clone();
        global_runtime()
            .block_on(async {
                let mut guard = gateway.lock().await;
                guard.stop().await
            })
            .map_err(FfiError::from)
    }
}

/// A proxy that connects a local MCP client to a remote Nostr MCP server.
#[derive(uniffi::Object)]
pub struct Proxy {
    proxy: Arc<tokio::sync::Mutex<contextvm_sdk::proxy::NostrMCPProxy>>,
    receiver: Arc<
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::JsonRpcMessage>>,
    >,
}

#[uniffi::export]
impl Proxy {
    /// Create and start a proxy transport.
    #[uniffi::constructor]
    pub fn new(keys: &Keys, config: &ClientConfig) -> Result<Self, FfiError> {
        let sdk_config = build_sdk_client_config_from_fields(ClientConfigParts {
            relay_urls: config.relay_urls.clone(),
            server_pubkey: config.server_pubkey.clone(),
            encryption_mode: sdk_encryption_mode(config.encryption_mode),
            gift_wrap_mode: sdk_gift_wrap_mode(config.gift_wrap_mode),
            is_stateless: config.is_stateless,
            timeout_secs: config.timeout_secs,
            discovery_relay_urls: config.discovery_relay_urls.clone(),
            fallback_operational_relay_urls: config.fallback_operational_relay_urls.clone(),
        });
        let proxy_config = contextvm_sdk::proxy::ProxyConfig::new(sdk_config);

        global_runtime()
            .block_on(async {
                let mut proxy =
                    contextvm_sdk::proxy::NostrMCPProxy::new(keys.inner.clone(), proxy_config)
                        .await?;
                let receiver = proxy.start().await?;
                Ok::<_, contextvm_sdk::Error>(Self {
                    proxy: Arc::new(tokio::sync::Mutex::new(proxy)),
                    receiver: Arc::new(tokio::sync::Mutex::new(receiver)),
                })
            })
            .map_err(FfiError::from)
    }

    /// Send a JSON-RPC message through the proxy.
    pub fn send(&self, payload_json: &str) -> Result<(), FfiError> {
        let message = parse_json_rpc(payload_json)?;
        let proxy = self.proxy.clone();
        global_runtime()
            .block_on(async {
                let guard = proxy.lock().await;
                guard.send(&message).await
            })
            .map_err(FfiError::from)
    }

    /// Receive the next response or notification.
    pub fn recv(&self) -> Result<JsonRpcMessage, FfiError> {
        recv_message(self.receiver.clone())
    }

    /// Receive the next response or notification, timing out after `timeout_secs`.
    pub fn recv_timeout(&self, timeout_secs: u64) -> Result<JsonRpcMessage, FfiError> {
        recv_message_timeout(self.receiver.clone(), timeout_secs)
    }

    /// Return the next response or notification if one is already buffered.
    pub fn recv_try(&self) -> Result<Option<JsonRpcMessage>, FfiError> {
        recv_message_try(self.receiver.clone())
    }

    /// Check if the proxy is active.
    pub fn is_active(&self) -> bool {
        let proxy = self.proxy.clone();
        global_runtime().block_on(async {
            let guard = proxy.lock().await;
            guard.is_active()
        })
    }

    /// Stop the proxy transport.
    pub fn stop(&self) -> Result<(), FfiError> {
        let proxy = self.proxy.clone();
        global_runtime()
            .block_on(async {
                let mut guard = proxy.lock().await;
                guard.stop().await
            })
            .map_err(FfiError::from)
    }
}

/// Discovery functions.
#[derive(uniffi::Object)]
pub struct Discovery;

#[uniffi::export]
impl Discovery {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self
    }

    /// Discover MCP servers on the given relay URLs.
    pub fn discover_servers(
        &self,
        pool: &RelayPool,
        relay_urls: Vec<String>,
    ) -> Result<Vec<ServerAnnouncement>, FfiError> {
        let client = pool.inner.client();
        global_runtime()
            .block_on(async {
                contextvm_sdk::discovery::discover_servers(client, &relay_urls).await
            })
            .map(|announcements| {
                announcements
                    .into_iter()
                    .map(|a| ServerAnnouncement {
                        pubkey: a.pubkey,
                        name: a.server_info.name,
                        version: a.server_info.version,
                        picture: a.server_info.picture,
                        about: a.server_info.about,
                        website: a.server_info.website,
                        event_id: a.event_id.to_hex(),
                    })
                    .collect()
            })
            .map_err(FfiError::from)
    }

    /// Discover MCP tools published by a specific provider.
    pub fn discover_tools(
        &self,
        pool: &RelayPool,
        provider_pubkey: String,
        provider_display_name: Option<String>,
        relay_urls: Vec<String>,
    ) -> Result<Vec<DiscoveredTool>, FfiError> {
        let client = pool.inner.client();
        global_runtime()
            .block_on(async {
                crate::discovery::discover_tools(
                    client,
                    &provider_pubkey,
                    provider_display_name,
                    &relay_urls,
                )
                .await
            })
            .map(|tools| tools.into_iter().map(tool_to_uniffi).collect())
            .map_err(FfiError::from)
    }

    /// Discover server announcements, tools, and provider profiles in one pass.
    pub fn discover_all_tools(
        &self,
        pool: &RelayPool,
        relay_urls: Vec<String>,
    ) -> Result<Vec<DiscoveredTool>, FfiError> {
        let client = pool.inner.client();
        global_runtime()
            .block_on(async { crate::discovery::discover_all_tools(client, &relay_urls).await })
            .map(|tools| tools.into_iter().map(tool_to_uniffi).collect())
            .map_err(FfiError::from)
    }

    /// Fetch Nostr kind-0 provider profiles for a set of provider pubkeys.
    pub fn fetch_provider_profiles(
        &self,
        pool: &RelayPool,
        provider_pubkeys: Vec<String>,
        relay_urls: Vec<String>,
    ) -> Result<Vec<ProviderProfile>, FfiError> {
        let client = pool.inner.client();
        global_runtime()
            .block_on(async {
                crate::discovery::fetch_provider_profiles(client, &provider_pubkeys, &relay_urls)
                    .await
            })
            .map(|profiles| profiles.into_values().map(profile_to_uniffi).collect())
            .map_err(FfiError::from)
    }
}

impl Default for Discovery {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Top-level functions ───────────────────────────────────────────────

/// Get the library version.
#[uniffi::export]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Convert a hex public key to npub bech32.
#[uniffi::export]
pub fn pubkey_hex_to_npub(pubkey_hex: String) -> Result<String, FfiError> {
    crate::discovery::pubkey_hex_to_npub(&pubkey_hex).map_err(FfiError::from)
}

/// Helper: build a JSON-RPC request as a JSON string.
#[uniffi::export]
pub fn make_request(id: String, method: String, params: Option<String>) -> String {
    let msg = contextvm_sdk::JsonRpcMessage::Request(contextvm_sdk::JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        method,
        params: params.and_then(|p| serde_json::from_str(&p).ok()),
    });
    serde_json::to_string(&msg).unwrap_or_default()
}

/// Helper: build a JSON-RPC notification as a JSON string.
#[uniffi::export]
pub fn make_notification(method: String, params: Option<String>) -> String {
    let msg = contextvm_sdk::JsonRpcMessage::Notification(contextvm_sdk::JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method,
        params: params.and_then(|p| serde_json::from_str(&p).ok()),
    });
    serde_json::to_string(&msg).unwrap_or_default()
}

/// Helper: build a JSON-RPC response as a JSON string.
#[uniffi::export]
pub fn make_response(id: String, result: String) -> String {
    let msg = contextvm_sdk::JsonRpcMessage::Response(contextvm_sdk::JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(id),
        result: serde_json::from_str(&result).unwrap_or(serde_json::json!(null)),
    });
    serde_json::to_string(&msg).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> contextvm_sdk::JsonRpcMessage {
        contextvm_sdk::JsonRpcMessage::Request(contextvm_sdk::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!("1"),
            method: "ping".to_string(),
            params: None,
        })
    }

    #[test]
    fn recv_message_try_returns_none_then_buffered_message() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Arc::new(tokio::sync::Mutex::new(rx));

        assert!(recv_message_try(rx.clone()).unwrap().is_none());
        tx.send(request()).unwrap();

        let msg = recv_message_try(rx).unwrap().unwrap();
        assert_eq!(msg.method, "ping");
    }

    #[test]
    fn recv_message_timeout_reports_timeout() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let err = recv_message_timeout(Arc::new(tokio::sync::Mutex::new(rx)), 0).unwrap_err();

        assert_eq!(err.code, crate::error::ErrorCode::Timeout);
    }

    #[test]
    fn recv_message_timeout_includes_lock_wait() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let _guard = global_runtime().block_on(rx.lock());

        let err = recv_message_timeout(rx.clone(), 0).unwrap_err();

        assert_eq!(err.code, crate::error::ErrorCode::Timeout);
    }

    #[test]
    fn recv_message_try_does_not_wait_for_lock() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let _guard = global_runtime().block_on(rx.lock());

        assert!(recv_message_try(rx.clone()).unwrap().is_none());
    }
}
