//! UniFFI-compatible types and high-level object-oriented API.
//!
//! These types are exposed via UniFFI proc-macros and provide a more ergonomic
//! interface than the flat C API.  They are designed for Python, Swift, and
//! Kotlin consumers.

use crate::builders::{
    build_sdk_client_config_from_fields, build_sdk_server_config_from_fields,
    build_server_config_parts, ClientConfigParts,
};
use crate::error::FfiError;
use crate::payment_gate::{PaymentGate, PaymentGateConfig, PaymentGateTransport};
use crate::runtime::global_runtime;
use crate::types::json_rpc_id_to_string;
use contextvm_sdk::transport::server::{PaymentNotificationSender, TargetedResponseSender};
use parking_lot::Mutex as ParkingLotMutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
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

/// CEP-8 server payment-interaction policy.
///
/// `Optional` lets clients negotiate `explicit_gating`.
/// `Transparent` rejects `explicit_gating` with a JSON-RPC `-32602`.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum PaymentInteractionPolicy {
    Optional,
    Transparent,
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
///
/// NOTE: the SDK's `contextvm_sdk::IncomingRequest` also carries an
/// `event: Option<nostr_sdk::Event>` field (the full client-signed event). It
/// is intentionally NOT mirrored here: no FFI consumer needs the raw Nostr
/// event today, and `nostr_sdk::Event` (with its `Tags` collection) is
/// non-trivial to expose across the C ABI + UniFFI. Add an `Event` mirror and a
/// field here when a foreign consumer needs `sig` / event binding.
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
    pub supports_open_stream: bool,
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
    /// `None` lets `Server::start()` derive a payment-aware default.
    /// Some(v) uses the explicit value (validated against payment budget when payments are enabled).
    pub session_timeout_secs: Option<u64>,
    pub cleanup_interval_secs: u64,
    pub excluded_capabilities: Vec<CapabilityExclusion>,
    pub max_sessions: u64,
    /// `None` lets `Server::start()` derive a payment-aware default.
    /// Some(v) uses the explicit value (validated against payment budget when payments are enabled).
    pub request_timeout_secs: Option<u64>,
    pub relay_list_urls: Vec<String>,
    pub bootstrap_relay_urls: Vec<String>,
    pub publish_relay_list: bool,
    pub profile_metadata_json: Option<String>,
    /// Maximum payment invoice TTL (seconds) the operator is willing to wait for.
    /// Used by `Server::start()` to derive request/session timeouts when payments are enabled.
    pub payment_ttl_cap_secs: u64,
    /// Estimated maximum execution budget (seconds) for a paid request.
    /// Used by `Server::start()` to derive request/session timeouts when payments are enabled.
    pub execution_budget_secs: u64,
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
            session_timeout_secs: None,
            cleanup_interval_secs: 60,
            excluded_capabilities: vec![],
            max_sessions: 1000,
            request_timeout_secs: None,
            relay_list_urls: vec![],
            bootstrap_relay_urls: vec![],
            publish_relay_list: true,
            profile_metadata_json: None,
            payment_ttl_cap_secs: crate::builders::DEFAULT_PAYMENT_TTL_CAP_SECS,
            execution_budget_secs: crate::builders::DEFAULT_EXECUTION_BUDGET_SECS,
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

fn sdk_payment_interaction_policy(
    policy: PaymentInteractionPolicy,
) -> contextvm_sdk::payments::PaymentInteractionPolicy {
    match policy {
        PaymentInteractionPolicy::Optional => {
            contextvm_sdk::payments::PaymentInteractionPolicy::Optional
        }
        PaymentInteractionPolicy::Transparent => {
            contextvm_sdk::payments::PaymentInteractionPolicy::Transparent
        }
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
        // req.event (the full client-signed Nostr event) is deliberately not
        // forwarded — see the note on `IncomingRequest` above.
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
        supports_open_stream: caps.supports_open_stream,
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

/// Supported payment method identifiers for CEP-8 payment requests.
#[allow(dead_code)]
pub(crate) const SUPPORTED_PAYMENT_METHOD_IDS: &[&str] = &["bitcoin-lightning-bolt11"];

/// Parse a JSON array of priced capabilities, validating each row.
fn parse_priced_capabilities_json(
    json: &str,
) -> Result<Vec<contextvm_sdk::payments::types::PricedCapability>, FfiError> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|e| FfiError {
        code: crate::error::ErrorCode::Serialization,
        message: format!("invalid priced_capabilities_json: {e}"),
    })?;

    rows.into_iter()
        .enumerate()
        .map(|(i, row)| parse_priced_capability_row(i, row))
        .collect()
}

fn parse_priced_capability_row(
    index: usize,
    row: serde_json::Value,
) -> Result<contextvm_sdk::payments::types::PricedCapability, FfiError> {
    let obj = row.as_object().ok_or_else(|| FfiError {
        code: crate::error::ErrorCode::Validation,
        message: format!("priced capability[{index}] is not an object"),
    })?;

    let method = obj
        .get("method")
        .and_then(|v| v.as_str())
        .ok_or_else(|| FfiError {
            code: crate::error::ErrorCode::Validation,
            message: format!("priced capability[{index}] missing 'method'"),
        })?
        .to_string();

    let name = obj.get("name").and_then(|v| v.as_str()).map(String::from);

    let amount = obj
        .get("amount")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| FfiError {
            code: crate::error::ErrorCode::Validation,
            message: format!("priced capability[{index}] missing or non-integer 'amount'"),
        })?;
    if amount <= 0 {
        return Err(FfiError {
            code: crate::error::ErrorCode::Validation,
            message: format!("priced capability[{index}] 'amount' must be positive"),
        });
    }

    let max_amount = match obj.get("maxAmount") {
        Some(v) => {
            let max = v.as_i64().ok_or_else(|| FfiError {
                code: crate::error::ErrorCode::Validation,
                message: format!("priced capability[{index}] 'maxAmount' must be an integer"),
            })?;
            if max < amount {
                return Err(FfiError {
                    code: crate::error::ErrorCode::Validation,
                    message: format!(
                        "priced capability[{index}] 'maxAmount' ({max}) must be >= 'amount' ({amount})"
                    ),
                });
            }
            Some(max)
        }
        None => None,
    };

    let currency_unit = obj
        .get("currencyUnit")
        .and_then(|v| v.as_str())
        .ok_or_else(|| FfiError {
            code: crate::error::ErrorCode::Validation,
            message: format!("priced capability[{index}] missing 'currencyUnit'"),
        })?;
    if currency_unit != "sats" {
        return Err(FfiError {
            code: crate::error::ErrorCode::Validation,
            message: format!(
                "priced capability[{index}] unsupported 'currencyUnit': {currency_unit} (only 'sats' is supported)"
            ),
        });
    }

    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(contextvm_sdk::payments::types::PricedCapability {
        method,
        name,
        amount,
        max_amount,
        currency_unit: currency_unit.to_string(),
        description,
    })
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
///
/// `Server` now has a two-phase lifecycle:
/// 1. **Configuring** — construct with `new`, call pre-start setters.
/// 2. **Started** — call `start()` to build the transport and begin listening.
/// 3. **Closed** — call `close()` to shut down.
#[derive(uniffi::Object)]
pub struct Server {
    state: AtomicU8,
    keys: contextvm_sdk::signer::Keys,
    inner: ParkingLotMutex<ServerInner>,
}

struct ServerInner {
    /// Pending configuration supplied to `new()` and mutated by pre-start setters.
    pending_config: ServerConfig,
    /// Announcement tags set before `start()`.
    extra_tags: Vec<nostr_sdk::prelude::Tag>,
    /// Pricing announcement tags set before `start()`.
    pricing_tags: Vec<nostr_sdk::prelude::Tag>,
    /// Parsed priced capabilities; a non-empty list at `start()` enables payment handling.
    priced_capabilities: Vec<contextvm_sdk::payments::types::PricedCapability>,
    /// CEP-8 payment interaction policy applied inside `start()`.
    payment_policy: Option<contextvm_sdk::payments::PaymentInteractionPolicy>,
    /// Live transport after `start()`.
    transport: Option<Arc<tokio::sync::Mutex<contextvm_sdk::NostrServerTransport>>>,
    /// Request receiver after `start()`.
    receiver: Option<
        Arc<
            tokio::sync::Mutex<
                tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::IncomingRequest>,
            >,
        >,
    >,
    /// Payment gate after `start()` if priced capabilities are non-empty.
    payment_gate: Option<crate::payment_gate::PaymentGate>,
    /// Optional override for the relay pool (tests only).
    relay_pool_override: Option<Arc<dyn contextvm_sdk::relay::RelayPoolTrait>>,
}

const STATE_CONFIGURING: u8 = 0;
const STATE_STARTING: u8 = 1;
const STATE_STARTED: u8 = 2;
const STATE_CLOSED: u8 = 3;

/// Real transport bridge used by the payment gate.
///
/// Captures the SDK's injectable notification and targeted-response senders so
/// the gate can emit CEP-8 lifecycle events and explicit-gating errors.
#[derive(Clone)]
struct ServerPaymentTransport {
    notification_sender: PaymentNotificationSender,
    targeted_sender: TargetedResponseSender,
}

impl PaymentGateTransport for ServerPaymentTransport {
    fn send_payment_notification(
        &self,
        client_pubkey: String,
        request_event_id: String,
        mirrored_wrap_kind: Option<u16>,
        notification: contextvm_sdk::JsonRpcMessage,
    ) -> Pin<Box<dyn Future<Output = contextvm_sdk::Result<()>> + Send>> {
        (self.notification_sender)(
            client_pubkey,
            request_event_id,
            mirrored_wrap_kind,
            notification,
        )
    }

    fn send_targeted_response(
        &self,
        client_pubkey: String,
        request_event_id: String,
        response: contextvm_sdk::JsonRpcMessage,
    ) -> Pin<Box<dyn Future<Output = contextvm_sdk::Result<()>> + Send>> {
        (self.targeted_sender)(client_pubkey, request_event_id, response)
    }
}

impl Server {
    fn not_started_error() -> FfiError {
        FfiError {
            code: crate::error::ErrorCode::NotStarted,
            message: "server has not been started".into(),
        }
    }

    fn closed_error() -> FfiError {
        FfiError {
            code: crate::error::ErrorCode::Closed,
            message: "server is closed".into(),
        }
    }

    fn not_configuring_error() -> FfiError {
        FfiError {
            code: crate::error::ErrorCode::Validation,
            message: "server is not in the configuring state".into(),
        }
    }

    fn start_state_error(current: u8) -> FfiError {
        match current {
            STATE_STARTED => FfiError {
                code: crate::error::ErrorCode::Validation,
                message: "server already started".into(),
            },
            STATE_STARTING => FfiError {
                code: crate::error::ErrorCode::Validation,
                message: "server is already starting".into(),
            },
            STATE_CLOSED => Self::closed_error(),
            _ => FfiError {
                code: crate::error::ErrorCode::Validation,
                message: "server cannot be started from its current state".into(),
            },
        }
    }

    fn require_started(&self) -> Result<(), FfiError> {
        match self.state.load(Ordering::SeqCst) {
            STATE_STARTED => Ok(()),
            STATE_CLOSED => Err(Self::closed_error()),
            _ => Err(Self::not_started_error()),
        }
    }

    fn require_configuring(&self) -> Result<(), FfiError> {
        match self.state.load(Ordering::SeqCst) {
            STATE_CONFIGURING => Ok(()),
            STATE_CLOSED => Err(Self::closed_error()),
            _ => Err(Self::not_configuring_error()),
        }
    }

    fn transport_ref(
        &self,
    ) -> Option<Arc<tokio::sync::Mutex<contextvm_sdk::NostrServerTransport>>> {
        self.inner.lock().transport.clone()
    }

    fn receiver_ref(
        &self,
    ) -> Option<
        Arc<
            tokio::sync::Mutex<
                tokio::sync::mpsc::UnboundedReceiver<contextvm_sdk::IncomingRequest>,
            >,
        >,
    > {
        self.inner.lock().receiver.clone()
    }

    fn no_payment_gate_error() -> FfiError {
        FfiError {
            code: crate::error::ErrorCode::Payment,
            message: "payments not configured".into(),
        }
    }

    /// Test-only constructor that injects a mock relay pool.
    #[doc(hidden)]
    pub fn new_with_relay_pool(
        keys: &Keys,
        config: &ServerConfig,
        relay_pool: Arc<dyn contextvm_sdk::relay::RelayPoolTrait>,
    ) -> Result<Self, FfiError> {
        let server = Self::new(keys, config)?;
        server.inner.lock().relay_pool_override = Some(relay_pool);
        Ok(server)
    }
}

#[uniffi::export]
impl Server {
    /// Create a server but do not start it yet.
    ///
    /// The returned `Server` is in the `Configuring` state. Call pre-start setters,
    /// then `start()` to begin listening.
    #[uniffi::constructor]
    pub fn new(keys: &Keys, config: &ServerConfig) -> Result<Self, FfiError> {
        // Fail fast on obviously invalid configuration, but do not build the transport
        // or open any relay connections until `start()`.
        build_sdk_server_config_from_fields(build_server_config_parts(config))?;

        Ok(Self {
            state: AtomicU8::new(STATE_CONFIGURING),
            keys: keys.inner.clone(),
            inner: ParkingLotMutex::new(ServerInner {
                pending_config: config.clone(),
                extra_tags: Vec::new(),
                pricing_tags: Vec::new(),
                priced_capabilities: Vec::new(),
                payment_policy: None,
                transport: None,
                receiver: None,
                payment_gate: None,
                relay_pool_override: None,
            }),
        })
    }

    /// Start the server transport.
    ///
    /// This applies the pending configuration, derives payment-adjusted timeouts
    /// if payments are enabled, builds the transport, and begins listening.
    /// May only be called once from the `Configuring` state.
    pub fn start(&self) -> Result<(), FfiError> {
        let prev = self.state.compare_exchange(
            STATE_CONFIGURING,
            STATE_STARTING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        match prev {
            Ok(_) => {}
            Err(current) => return Err(Self::start_state_error(current)),
        }

        // Snapshot pending configuration; no setters can run while we are STARTING.
        let (
            pending_config,
            extra_tags,
            pricing_tags,
            priced_capabilities,
            payment_policy,
            relay_pool_override,
        ) = {
            let inner = self.inner.lock();
            (
                inner.pending_config.clone(),
                inner.extra_tags.clone(),
                inner.pricing_tags.clone(),
                inner.priced_capabilities.clone(),
                inner.payment_policy,
                inner.relay_pool_override.clone(),
            )
        };

        let mut sdk_config =
            build_sdk_server_config_from_fields(build_server_config_parts(&pending_config))?;

        let payment_enabled = !priced_capabilities.is_empty() || payment_policy.is_some();
        let ttl_cap = pending_config.payment_ttl_cap_secs;
        let exec = pending_config.execution_budget_secs;
        let margin = crate::builders::PAYMENT_BUDGET_MARGIN_SECS;
        let lower_bound = crate::builders::payment_timeout_lower_bound(ttl_cap, exec, margin);

        let request = match pending_config.request_timeout_secs {
            Some(v) if v > 0 => {
                if payment_enabled {
                    crate::builders::validate_explicit_timeout(v, lower_bound)?;
                }
                v
            }
            _ => {
                if payment_enabled {
                    crate::builders::derive_payment_request_timeout(ttl_cap, exec, margin)
                } else {
                    sdk_config.request_timeout.as_secs()
                }
            }
        };
        let session = match pending_config.session_timeout_secs {
            Some(v) if v > 0 => {
                if payment_enabled {
                    crate::builders::validate_explicit_timeout(v, lower_bound)?;
                }
                v
            }
            _ => {
                if payment_enabled {
                    crate::builders::derive_payment_session_timeout(ttl_cap, exec, margin)
                } else {
                    sdk_config.session_timeout.as_secs()
                }
            }
        };
        sdk_config.request_timeout = Duration::from_secs(request);
        sdk_config.session_timeout = Duration::from_secs(session);

        let mut payment_extra = extra_tags;
        if payment_enabled {
            payment_extra.extend(contextvm_sdk::payments::tags::pmi_tags(&[
                contextvm_sdk::payments::constants::PMI_BITCOIN_LIGHTNING_BOLT11.into(),
            ]));
            if matches!(
                payment_policy,
                Some(contextvm_sdk::payments::PaymentInteractionPolicy::Optional)
            ) {
                payment_extra.push(contextvm_sdk::payments::tags::payment_interaction_tag(
                    contextvm_sdk::core::types::PaymentInteractionMode::ExplicitGating,
                ));
            }
        }

        let gate_caps: Vec<crate::payment_gate::PricedCapability> =
            priced_capabilities.iter().map(|c| c.into()).collect();
        let gate_config = PaymentGateConfig {
            payment_ttl_cap_secs: ttl_cap,
            execution_budget_secs: exec,
            request_timeout_secs: request,
            session_timeout_secs: session,
            parked_cap: 128,
            event_queue_bound: 64,
            policy: crate::payment_gate::PaymentLifecyclePolicy::Transparent,
            priced_capabilities: gate_caps,
        };

        let result = global_runtime().block_on(async move {
            let mut transport = if let Some(pool) = relay_pool_override {
                contextvm_sdk::NostrServerTransport::with_relay_pool(sdk_config, pool).await
            } else {
                contextvm_sdk::NostrServerTransport::new(self.keys.clone(), sdk_config).await
            }
            .map_err(FfiError::from)?;

            if !payment_extra.is_empty() {
                transport.set_announcement_extra_tags(payment_extra);
            }
            if !pricing_tags.is_empty() {
                transport.set_announcement_pricing_tags(pricing_tags);
            }
            if let Some(policy) = payment_policy {
                transport.set_supported_payment_interaction(policy);
            }

            // Build and register the payment gate before start() so its senders capture
            // the tag sets set above and the middleware chain is frozen in place.
            let payment_gate = if !priced_capabilities.is_empty() {
                let notification_sender =
                    transport.payment_notification_sender(Duration::from_secs(ttl_cap));
                let targeted_sender = transport.targeted_response_sender();
                let gate_transport = Arc::new(ServerPaymentTransport {
                    notification_sender,
                    targeted_sender,
                });
                let gate = PaymentGate::new(gate_config, gate_transport)?;
                let middleware: Arc<dyn contextvm_sdk::transport::server::InboundMiddleware> =
                    Arc::new(gate.clone());
                transport.add_inbound_middleware(middleware);
                Some(gate)
            } else {
                None
            };

            transport.start().await.map_err(FfiError::from)?;
            transport.spawn_discoverability_publication();

            Ok::<_, FfiError>((transport, payment_gate))
        });

        match result {
            Ok((mut transport, payment_gate)) => {
                let receiver = transport.take_message_receiver().ok_or_else(|| FfiError {
                    code: crate::error::ErrorCode::Other,
                    message: "receiver already taken".into(),
                })?;
                let mut inner = self.inner.lock();
                inner.transport = Some(Arc::new(tokio::sync::Mutex::new(transport)));
                inner.receiver = Some(Arc::new(tokio::sync::Mutex::new(receiver)));
                inner.payment_gate = payment_gate;
                self.state.store(STATE_STARTED, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                self.state.store(STATE_CONFIGURING, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// Receive the next incoming request.  Blocks until one is available.
    pub fn recv(&self) -> Result<IncomingRequest, FfiError> {
        self.require_started()?;
        let receiver = self.receiver_ref().ok_or_else(Self::not_started_error)?;
        recv_incoming(receiver)
    }

    /// Receive the next incoming request, timing out after `timeout_secs`.
    pub fn recv_timeout(&self, timeout_secs: u64) -> Result<IncomingRequest, FfiError> {
        self.require_started()?;
        let receiver = self.receiver_ref().ok_or_else(Self::not_started_error)?;
        recv_incoming_timeout(receiver, timeout_secs)
    }

    /// Return the next incoming request if one is already buffered.
    pub fn recv_try(&self) -> Result<Option<IncomingRequest>, FfiError> {
        self.require_started()?;
        let receiver = self.receiver_ref().ok_or_else(Self::not_started_error)?;
        recv_incoming_try(receiver)
    }

    /// Send a response for a given event ID.
    pub fn send_response(&self, event_id: &str, payload_json: &str) -> Result<(), FfiError> {
        self.require_started()?;
        let message = parse_json_rpc(payload_json)?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let message = parse_json_rpc(payload_json)?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let message = parse_json_rpc(payload_json)?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.broadcast_notification(&message).await
            })
            .map_err(FfiError::from)
    }

    /// Sets extra announcement/discovery tags from a JSON array of tag arrays.
    pub fn set_announcement_extra_tags(&self, tags_json: &str) -> Result<(), FfiError> {
        self.require_configuring()?;
        let tags = parse_tags_json(tags_json)?;
        let mut inner = self.inner.lock();
        inner.extra_tags = tags;
        Ok(())
    }

    /// Sets pricing tags from a JSON array of tag arrays.
    pub fn set_announcement_pricing_tags(&self, tags_json: &str) -> Result<(), FfiError> {
        self.require_configuring()?;
        let tags = parse_tags_json(tags_json)?;
        let mut inner = self.inner.lock();
        inner.pricing_tags = tags;
        Ok(())
    }

    /// Register priced capabilities from a JSON array.
    ///
    /// Parsed and validated immediately, and the announcement `cap` pricing tags are
    /// derived from the same source of truth used by the payment gate.
    pub fn set_priced_capabilities_json(&self, json: &str) -> Result<(), FfiError> {
        self.require_configuring()?;
        let caps = parse_priced_capabilities_json(json)?;
        let tags = contextvm_sdk::payments::tags::cap_tags_from_priced_capabilities(&caps);
        let mut inner = self.inner.lock();
        inner.priced_capabilities = caps;
        inner.pricing_tags = tags;
        Ok(())
    }

    /// Set the supported CEP-8 payment-interaction policy.
    pub fn set_payment_interaction_policy(
        &self,
        policy: PaymentInteractionPolicy,
    ) -> Result<(), FfiError> {
        self.require_configuring()?;
        let mut inner = self.inner.lock();
        inner.payment_policy = Some(sdk_payment_interaction_policy(policy));
        Ok(())
    }

    /// Receive the next payment-gate request, timing out after `timeout_secs`.
    ///
    /// Returns `None` on timeout (or when no payment gate is configured).
    pub fn recv_payment_gate_request(
        &self,
        timeout_secs: u64,
    ) -> Result<Option<crate::payment_gate::PaymentGateRequest>, FfiError> {
        self.require_started()?;
        let gate = self.inner.lock().payment_gate.clone();
        match gate {
            Some(gate) => Ok(global_runtime()
                .block_on(async { gate.recv_timeout(Duration::from_secs(timeout_secs)).await })),
            None => Ok(None),
        }
    }

    /// Submit an invoice for a parked payment-gate request.
    pub fn submit_invoice(
        &self,
        request_event_id: String,
        amount_sats: i64,
        pay_req: String,
        pmi: String,
        ttl_secs: u64,
        description: Option<String>,
    ) -> Result<(), FfiError> {
        self.require_started()?;
        let gate = self
            .inner
            .lock()
            .payment_gate
            .clone()
            .ok_or_else(Self::no_payment_gate_error)?;
        global_runtime().block_on(async {
            gate.submit_invoice(
                &request_event_id,
                amount_sats,
                &pay_req,
                &pmi,
                ttl_secs,
                description.as_deref(),
            )
            .await
        })
    }

    /// Mark a previously submitted invoice as settled.
    pub fn mark_payment_settled(
        &self,
        pay_req: String,
        meta_json: Option<String>,
    ) -> Result<(), FfiError> {
        self.require_started()?;
        let gate = self
            .inner
            .lock()
            .payment_gate
            .clone()
            .ok_or_else(Self::no_payment_gate_error)?;
        global_runtime().block_on(async { gate.mark_settled(&pay_req, meta_json.as_deref()).await })
    }

    /// Mark a previously submitted invoice as failed.
    pub fn mark_payment_failed(
        &self,
        pay_req: String,
        message: Option<String>,
    ) -> Result<(), FfiError> {
        self.require_started()?;
        let gate = self
            .inner
            .lock()
            .payment_gate
            .clone()
            .ok_or_else(Self::no_payment_gate_error)?;
        let message = message.as_deref().unwrap_or("");
        global_runtime().block_on(async { gate.mark_failed(&pay_req, message).await })
    }

    /// Mark a parked paid request as already completed so it can be replayed for free.
    pub fn mark_replayed(&self, request_event_id: String) -> Result<(), FfiError> {
        self.require_started()?;
        let gate = self
            .inner
            .lock()
            .payment_gate
            .clone()
            .ok_or_else(Self::no_payment_gate_error)?;
        global_runtime().block_on(async { gate.mark_replayed(&request_event_id).await })
    }

    /// Publish server announcement.
    pub fn announce(&self) -> Result<(), FfiError> {
        self.require_started()?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let tools = parse_json_value_array(tools_json, "tools_json")?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let resources = parse_json_value_array(resources_json, "resources_json")?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let prompts = parse_json_value_array(prompts_json, "prompts_json")?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let templates = parse_json_value_array(templates_json, "templates_json")?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
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
        self.require_started()?;
        let transport = self.transport_ref().ok_or_else(Self::not_started_error)?;
        global_runtime()
            .block_on(async {
                let guard = transport.lock().await;
                guard.delete_announcements(reason).await
            })
            .map_err(FfiError::from)
    }

    /// Close the server transport.
    pub fn close(&self) -> Result<(), FfiError> {
        match self.state.load(Ordering::SeqCst) {
            STATE_CLOSED => return Ok(()),
            STATE_CONFIGURING => {
                self.state.store(STATE_CLOSED, Ordering::SeqCst);
                return Ok(());
            }
            _ => {}
        }

        let transport = {
            let mut inner = self.inner.lock();
            inner.transport.take()
        };
        self.state.store(STATE_CLOSED, Ordering::SeqCst);

        if let Some(transport) = transport {
            global_runtime()
                .block_on(async {
                    let mut guard = transport.lock().await;
                    guard.close().await
                })
                .map_err(FfiError::from)?;
        }

        let mut inner = self.inner.lock();
        inner.receiver = None;
        inner.payment_gate = None;
        Ok(())
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
        let sdk_config = build_sdk_server_config_from_fields(build_server_config_parts(config))?;
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

    #[test]
    fn server_config_default_uses_payment_budget_defaults() {
        let config = ServerConfig::default();
        assert!(config.session_timeout_secs.is_none());
        assert!(config.request_timeout_secs.is_none());
        assert_eq!(config.payment_ttl_cap_secs, 300);
        assert_eq!(config.execution_budget_secs, 600);
    }

    #[test]
    fn payment_interaction_policy_maps_to_sdk() {
        assert_eq!(
            sdk_payment_interaction_policy(PaymentInteractionPolicy::Optional),
            contextvm_sdk::payments::PaymentInteractionPolicy::Optional
        );
        assert_eq!(
            sdk_payment_interaction_policy(PaymentInteractionPolicy::Transparent),
            contextvm_sdk::payments::PaymentInteractionPolicy::Transparent
        );
    }

    #[test]
    fn priced_capabilities_validation() {
        let good = r#"[{"method":"tools/call","amount":1000,"currencyUnit":"sats"}]"#;
        let caps = parse_priced_capabilities_json(good).unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].amount, 1000);
        assert_eq!(caps[0].currency_unit, "sats");

        let bad_unit = r#"[{"method":"tools/call","amount":1000,"currencyUnit":"msat"}]"#;
        let err = parse_priced_capabilities_json(bad_unit).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Validation);

        let zero_amount = r#"[{"method":"tools/call","amount":0,"currencyUnit":"sats"}]"#;
        let err = parse_priced_capabilities_json(zero_amount).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Validation);

        let max_bad =
            r#"[{"method":"tools/call","amount":1000,"maxAmount":500,"currencyUnit":"sats"}]"#;
        let err = parse_priced_capabilities_json(max_bad).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Validation);

        assert_eq!(SUPPORTED_PAYMENT_METHOD_IDS, &["bitcoin-lightning-bolt11"]);
    }

    #[test]
    fn payment_timeout_derivation() {
        use crate::builders;

        let lower = builders::payment_timeout_lower_bound(300, 600, 60);
        assert_eq!(lower, 960);
        let request = builders::derive_payment_request_timeout(300, 600, 60);
        assert_eq!(request, 1020);
        let session = builders::derive_payment_session_timeout(300, 600, 60);
        assert_eq!(session, 1080);

        assert!(builders::validate_explicit_timeout(1020, 960).is_ok());
        assert!(builders::validate_explicit_timeout(960, 960).is_err());
        assert!(builders::validate_explicit_timeout(900, 960).is_err());
    }

    #[test]
    fn server_state_machine_enforces_lifecycle() {
        use contextvm_sdk::relay::MockRelayPool;

        let keys = Keys::generate();
        let server_pool = MockRelayPool::with_keys(keys.inner.clone());
        let config = ServerConfig {
            relay_urls: vec![],
            ..Default::default()
        };
        let server = Server::new_with_relay_pool(&keys, &config, Arc::new(server_pool)).unwrap();

        // Setters work before start.
        server.set_announcement_extra_tags("[]").unwrap();
        server
            .set_payment_interaction_policy(PaymentInteractionPolicy::Optional)
            .unwrap();

        // Operations that need a started server are rejected.
        let err = server.recv_timeout(0).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::NotStarted);

        // Start succeeds.
        server.start().unwrap();

        // Second start is rejected.
        let err = server.start().unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Validation);

        // Setters after start are rejected.
        let err = server.set_announcement_extra_tags("[]").unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Validation);

        // Close succeeds and subsequent calls report Closed.
        server.close().unwrap();

        let err = server.recv_timeout(0).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Closed);

        let err = server.start().unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Closed);
    }

    #[test]
    fn server_close_before_start_is_allowed() {
        let keys = Keys::generate();
        let server = Server::new(&keys, &ServerConfig::default()).unwrap();

        server.close().unwrap();

        let err = server
            .set_payment_interaction_policy(PaymentInteractionPolicy::Optional)
            .unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Closed);
    }
}
