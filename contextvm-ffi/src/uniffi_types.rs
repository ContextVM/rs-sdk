//! UniFFI-compatible types and high-level object-oriented API.
//!
//! These types are exposed via UniFFI proc-macros and provide a more ergonomic
//! interface than the flat C API.  They are designed for Python, Swift, and
//! Kotlin consumers.

use crate::builders::{
    build_sdk_client_config_from_fields, build_sdk_server_config_from_fields, ServerConfigParts,
};
use crate::error::FfiError;
use crate::runtime::global_runtime;
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
        id: msg.id().map(|v| v.to_string()).unwrap_or_default(),
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
        });

        global_runtime()
            .block_on(async {
                let mut transport =
                    contextvm_sdk::NostrServerTransport::new(keys.inner.clone(), sdk_config)
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

    /// Receive the next incoming request.  Blocks until one is available.
    pub fn recv(&self) -> Result<IncomingRequest, FfiError> {
        let rx = self.receiver.clone();
        global_runtime()
            .block_on(async {
                let mut guard = rx.lock().await;
                guard.recv().await
            })
            .map(|req| incoming_to_uniffi(&req))
            .ok_or_else(|| FfiError {
                code: crate::error::ErrorCode::Transport,
                message: "channel closed".into(),
            })
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
        let sdk_config = build_sdk_client_config_from_fields(
            config.relay_urls.clone(),
            config.server_pubkey.clone(),
            sdk_encryption_mode(config.encryption_mode),
            sdk_gift_wrap_mode(config.gift_wrap_mode),
            config.is_stateless,
            config.timeout_secs,
        );

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
        let rx = self.receiver.clone();
        global_runtime()
            .block_on(async {
                let mut guard = rx.lock().await;
                guard.recv().await
            })
            .map(|msg| message_to_uniffi(&msg))
            .ok_or_else(|| FfiError {
                code: crate::error::ErrorCode::Transport,
                message: "channel closed".into(),
            })
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
        let sdk_config = build_sdk_client_config_from_fields(
            config.relay_urls.clone(),
            config.server_pubkey.clone(),
            sdk_encryption_mode(config.encryption_mode),
            sdk_gift_wrap_mode(config.gift_wrap_mode),
            config.is_stateless,
            config.timeout_secs,
        );
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
        let rx = self.receiver.clone();
        global_runtime()
            .block_on(async {
                let mut guard = rx.lock().await;
                guard.recv().await
            })
            .map(|msg| message_to_uniffi(&msg))
            .ok_or_else(|| FfiError {
                code: crate::error::ErrorCode::Transport,
                message: "channel closed".into(),
            })
    }

    /// Receive the next response or notification, timing out after `timeout_secs`.
    pub fn recv_timeout(&self, timeout_secs: u64) -> Result<JsonRpcMessage, FfiError> {
        let rx = self.receiver.clone();
        match global_runtime().block_on(async {
            let mut guard = rx.lock().await;
            tokio::time::timeout(Duration::from_secs(timeout_secs), guard.recv()).await
        }) {
            Ok(Some(msg)) => Ok(message_to_uniffi(&msg)),
            Ok(None) => Err(FfiError {
                code: crate::error::ErrorCode::Transport,
                message: "channel closed".into(),
            }),
            Err(_) => Err(FfiError {
                code: crate::error::ErrorCode::Timeout,
                message: "receive timed out".into(),
            }),
        }
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
