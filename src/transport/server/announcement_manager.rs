//! Announcement tag management for the server transport.
//!
//! Encapsulates tag composition, caching, and publishing for CEP-6 server
//! announcements (kinds 11316–11320) and CEP-35 first-response discovery.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::sync::Notify;

use super::IncomingRequest;
use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::*;
use crate::relay::RelayPoolTrait;

const LOG_TARGET: &str = "contextvm_sdk::transport::server::announcement";

/// Default timeout waiting for the rmcp handler to respond to the synthetic
/// initialize request during announcement auto-publish.
#[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
const ANNOUNCEMENT_INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Manages tag composition and publishing for server announcements.
///
/// Handles CEP-6 announcement event publishing (kinds 11316–11320) and
/// CEP-35 common tag composition for first-response discovery replay.
/// Tag accessors use interior-mutability caching so callers may hold `&self`.
pub(crate) struct AnnouncementManager {
    /// Shared relay pool for publishing announcement events.
    relay_pool: Arc<dyn RelayPoolTrait>,
    /// Server metadata for announcement tags.
    server_info: Option<ServerInfo>,
    /// Encryption mode — determines whether encryption tags are emitted.
    encryption_mode: EncryptionMode,
    /// Gift-wrap mode — determines whether ephemeral tag is emitted.
    gift_wrap_mode: GiftWrapMode,
    /// User-provided extra tags (e.g. PMI discovery for CEP-8).
    extra_common_tags: Vec<Tag>,
    /// Transport-owned internal tags (future CEP-22 oversized support signal).
    internal_common_tags: Vec<Tag>,
    /// CEP-8 pricing tags for capability list responses.
    pricing_tags: Vec<Tag>,
    /// Cached result of `get_common_tags()`. Invalidated by tag setters.
    cached_common_tags: Mutex<Option<Vec<Tag>>>,
    /// Channel for injecting synthetic MCP messages into the transport's inbound queue.
    /// Wrapped in `Option` so it can be dropped during `close()` — otherwise this
    /// clone keeps the message channel alive after `message_tx` is taken.
    dispatch_fn: Option<tokio::sync::mpsc::UnboundedSender<IncomingRequest>>,
    /// Notifier signaled when the announcement init response has been processed.
    init_notify: Arc<Notify>,
    /// Whether the announcement initialization has completed.
    /// Only read by `handle_announcement_response`, which is called from the
    /// rmcp worker — unused when the `rmcp` feature is disabled.
    #[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
    initialized: Mutex<bool>,
    /// Transport's connected relay URLs (fallback for `relay_list_urls`).
    relay_urls: Vec<String>,
    /// Explicit relay URLs to advertise in the kind 10002 relay list event.
    relay_list_urls: Option<Vec<String>>,
    /// Additional bootstrap relay URLs for discoverability publication.
    bootstrap_relay_urls: Option<Vec<String>>,
    /// Whether to publish a relay list event (kind 10002).
    should_publish_relay_list: bool,
    /// NIP-01 profile metadata (kind 0) to publish at startup.
    profile_metadata: Option<ProfileMetadata>,
}

/// Maps an MCP result schema to the Nostr event kind for announcement publishing.
///
/// The `matches` function validates the result's structure (not just field presence),
/// matching the TS SDK's Zod `safeParse` semantics.
struct AnnouncementMapping {
    matches: fn(&serde_json::Value) -> bool,
    kind: u16,
}

/// Schema-to-kind mapping table for MCP announcement responses.
///
/// Checked in order — first match wins. Validation checks field types:
/// `protocolVersion` must be a string, `capabilities` must be an object,
/// and capability lists (`tools`, `resources`, etc.) must be arrays.
const ANNOUNCEMENT_MAPPINGS: &[AnnouncementMapping] = &[
    AnnouncementMapping {
        matches: |r| r.get("protocolVersion").is_some_and(|v| v.is_string()),
        kind: SERVER_ANNOUNCEMENT_KIND,
    },
    AnnouncementMapping {
        matches: |r| r.get("capabilities").is_some_and(|v| v.is_object()),
        kind: SERVER_ANNOUNCEMENT_KIND,
    },
    AnnouncementMapping {
        matches: |r| r.get("tools").is_some_and(|v| v.is_array()),
        kind: TOOLS_LIST_KIND,
    },
    AnnouncementMapping {
        matches: |r| r.get("resources").is_some_and(|v| v.is_array()),
        kind: RESOURCES_LIST_KIND,
    },
    AnnouncementMapping {
        matches: |r| r.get("resourceTemplates").is_some_and(|v| v.is_array()),
        kind: RESOURCETEMPLATES_LIST_KIND,
    },
    AnnouncementMapping {
        matches: |r| r.get("prompts").is_some_and(|v| v.is_array()),
        kind: PROMPTS_LIST_KIND,
    },
];

/// Check whether a relay URL points to a local address.
///
/// Used for smart bootstrap relay detection: default bootstrap relays are
/// skipped when all advertised relays are local and no explicit bootstrap
/// URLs were provided.
fn is_local_relay_url(url: &str) -> bool {
    let without_proto = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    let lower = without_proto.to_lowercase();
    for prefix in &["localhost", "127.0.0.1", "0.0.0.0", "[::1]"] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with(':') || rest.starts_with('/') {
                return true;
            }
        }
    }
    false
}

impl AnnouncementManager {
    /// Create a new announcement manager.
    ///
    /// `dispatch_fn` is a clone of the transport's `message_tx` channel, used to
    /// inject synthetic MCP messages (initialize, notifications/initialized,
    /// capability list requests) during the auto-publish flow.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        relay_pool: Arc<dyn RelayPoolTrait>,
        server_info: Option<ServerInfo>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        dispatch_fn: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
        relay_urls: Vec<String>,
        relay_list_urls: Option<Vec<String>>,
        bootstrap_relay_urls: Option<Vec<String>>,
        should_publish_relay_list: bool,
        profile_metadata: Option<ProfileMetadata>,
    ) -> Self {
        Self {
            relay_pool,
            server_info,
            encryption_mode,
            gift_wrap_mode,
            extra_common_tags: Vec::new(),
            internal_common_tags: Vec::new(),
            pricing_tags: Vec::new(),
            cached_common_tags: Mutex::new(None),
            dispatch_fn: Some(dispatch_fn),
            init_notify: Arc::new(Notify::new()),
            initialized: Mutex::new(false),
            relay_urls,
            relay_list_urls,
            bootstrap_relay_urls,
            should_publish_relay_list,
            profile_metadata,
        }
    }

    // ── Tag accessors ──────────────────────────────────────────────

    /// Build server identity tags (name, about, website, picture).
    pub fn get_server_info_tags(&self) -> Vec<Tag> {
        let mut tags = Vec::new();
        if let Some(ref info) = self.server_info {
            if let Some(ref name) = info.name {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::NAME.into()),
                    vec![name.clone()],
                ));
            }
            if let Some(ref about) = info.about {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::ABOUT.into()),
                    vec![about.clone()],
                ));
            }
            if let Some(ref website) = info.website {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::WEBSITE.into()),
                    vec![website.clone()],
                ));
            }
            if let Some(ref picture) = info.picture {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::PICTURE.into()),
                    vec![picture.clone()],
                ));
            }
        }
        tags
    }

    /// Build capability tags based on encryption and gift-wrap mode.
    pub fn get_capability_tags(&self) -> Vec<Tag> {
        let mut tags = Vec::new();
        if self.encryption_mode != EncryptionMode::Disabled {
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_ENCRYPTION.into()),
                Vec::<String>::new(),
            ));
            if self.gift_wrap_mode.supports_ephemeral() {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
                    Vec::<String>::new(),
                ));
            }
        }
        tags
    }

    /// Returns combined common tags: server info + capability + extra + internal.
    ///
    /// Results are cached; the cache is invalidated when extra or internal
    /// common tags are updated via their setters.
    pub fn get_common_tags(&self) -> Vec<Tag> {
        let mut cache = self
            .cached_common_tags
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(ref cached) = *cache {
            return cached.clone();
        }
        let mut tags = self.get_server_info_tags();
        tags.extend(self.get_capability_tags());
        tags.extend(self.extra_common_tags.iter().cloned());
        tags.extend(self.internal_common_tags.iter().cloned());
        *cache = Some(tags.clone());
        tags
    }

    /// Returns a reference to the current pricing tags.
    #[allow(dead_code)] // API reserved for CEP-8 pricing integration
    pub fn get_pricing_tags(&self) -> &[Tag] {
        &self.pricing_tags
    }

    /// Build tags for a specific announcement kind.
    ///
    /// Kind 11316 (server announcement) receives common + pricing tags.
    /// Kinds 11317–11320 (capability lists) receive pricing tags only.
    pub fn get_announcement_tags(&self, kind: u16) -> Vec<Tag> {
        if kind == SERVER_ANNOUNCEMENT_KIND {
            let mut tags = self.get_common_tags();
            tags.extend(self.pricing_tags.iter().cloned());
            tags
        } else {
            self.pricing_tags.clone()
        }
    }

    // ── Setters ────────────────────────────────────────────────────

    /// Set user-provided extra common tags (e.g. PMI discovery for CEP-8).
    ///
    /// Invalidates the common tags cache.
    pub fn set_extra_common_tags(&mut self, tags: Vec<Tag>) {
        self.extra_common_tags = tags;
        *self
            .cached_common_tags
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Set transport-owned internal common tags (e.g. CEP-22 oversized support).
    ///
    /// Invalidates the common tags cache.
    #[allow(dead_code)] // API reserved for CEP-22 oversized transfer integration
    pub fn set_internal_common_tags(&mut self, tags: Vec<Tag>) {
        self.internal_common_tags = tags;
        *self
            .cached_common_tags
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Set CEP-8 pricing tags for capability list responses.
    ///
    /// Does not invalidate the common tags cache (pricing is separate).
    pub fn set_pricing_tags(&mut self, tags: Vec<Tag>) {
        self.pricing_tags = tags;
    }

    // ── Publish methods ────────────────────────────────────────────

    /// Publish server announcement (kind 11316).
    pub async fn announce(&self) -> Result<EventId> {
        let info = self
            .server_info
            .as_ref()
            .ok_or_else(|| Error::Other("No server info configured".to_string()))?;

        let content = serde_json::to_string(info)?;
        let tags = self.get_announcement_tags(SERVER_ANNOUNCEMENT_KIND);
        let builder = EventBuilder::new(Kind::Custom(SERVER_ANNOUNCEMENT_KIND), content).tags(tags);
        self.relay_pool.publish(builder).await
    }

    /// Publish tools list (kind 11317).
    pub async fn publish_tools(&self, tools: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "tools": tools });
        let builder = EventBuilder::new(
            Kind::Custom(TOOLS_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.relay_pool.publish(builder).await
    }

    /// Publish resources list (kind 11318).
    pub async fn publish_resources(&self, resources: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "resources": resources });
        let builder = EventBuilder::new(
            Kind::Custom(RESOURCES_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.relay_pool.publish(builder).await
    }

    /// Publish prompts list (kind 11320).
    pub async fn publish_prompts(&self, prompts: Vec<serde_json::Value>) -> Result<EventId> {
        let content = serde_json::json!({ "prompts": prompts });
        let builder = EventBuilder::new(
            Kind::Custom(PROMPTS_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.relay_pool.publish(builder).await
    }

    /// Publish resource templates list (kind 11319).
    pub async fn publish_resource_templates(
        &self,
        templates: Vec<serde_json::Value>,
    ) -> Result<EventId> {
        let content = serde_json::json!({ "resourceTemplates": templates });
        let builder = EventBuilder::new(
            Kind::Custom(RESOURCETEMPLATES_LIST_KIND),
            serde_json::to_string(&content)?,
        )
        .tags(self.pricing_tags.iter().cloned());
        self.relay_pool.publish(builder).await
    }

    /// Delete server announcements (NIP-09 kind 5).
    ///
    /// Queries existing announcement events per kind and publishes deletion
    /// events with `["e", event_id]` tags via `EventBuilder::delete()`.
    pub async fn delete_announcements(&self, reason: &str) -> Result<()> {
        let pubkey = self.relay_pool.public_key().await?;
        for &kind in UNENCRYPTED_KINDS {
            let filter = Filter::new().kind(Kind::Custom(kind)).author(pubkey);
            let events = self
                .relay_pool
                .fetch_events(filter, Duration::from_secs(10))
                .await?;
            if events.is_empty() {
                continue;
            }
            let request = EventDeletionRequest::new()
                .ids(events.iter().map(|e| e.id))
                .reason(reason);
            self.relay_pool
                .publish(EventBuilder::delete(request))
                .await?;
        }
        Ok(())
    }

    /// Publish tools list from rmcp typed tool descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_tools_typed(&self, tools: Vec<rmcp::model::Tool>) -> Result<EventId> {
        let tools = tools
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_tools(tools).await
    }

    /// Publish resources list from rmcp typed resource descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resources_typed(
        &self,
        resources: Vec<rmcp::model::Resource>,
    ) -> Result<EventId> {
        let resources = resources
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_resources(resources).await
    }

    /// Publish prompts list from rmcp typed prompt descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_prompts_typed(
        &self,
        prompts: Vec<rmcp::model::Prompt>,
    ) -> Result<EventId> {
        let prompts = prompts
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_prompts(prompts).await
    }

    /// Publish resource templates list from rmcp typed template descriptors.
    #[cfg(feature = "rmcp")]
    pub async fn publish_resource_templates_typed(
        &self,
        templates: Vec<rmcp::model::ResourceTemplate>,
    ) -> Result<EventId> {
        let templates = templates
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.publish_resource_templates(templates).await
    }

    // ── Relay list + profile metadata (CEP-6) ─────────────────────────

    /// Returns the relay URLs to advertise in the kind 10002 relay list event.
    ///
    /// Uses `relay_list_urls` if explicitly configured, otherwise falls back
    /// to the transport's connected `relay_urls`.
    pub(crate) fn get_advertised_relay_urls(&self) -> &[String] {
        self.relay_list_urls.as_deref().unwrap_or(&self.relay_urls)
    }

    /// Compute relay URLs for discoverability event publication.
    ///
    /// Merges advertised relay URLs with bootstrap relay URLs, deduplicated.
    /// Skips default bootstrap relays when all advertised URLs are local and
    /// no explicit `bootstrap_relay_urls` were provided.
    pub(crate) fn get_discoverability_publish_relay_urls(&self) -> Vec<String> {
        let advertised = self.get_advertised_relay_urls();
        let has_explicit_bootstrap = self.bootstrap_relay_urls.is_some();

        let should_skip_bootstrap = !has_explicit_bootstrap
            && !advertised.is_empty()
            && advertised.iter().all(|url| is_local_relay_url(url));

        let mut seen = HashSet::new();
        let mut result = Vec::new();

        for url in advertised {
            if seen.insert(url.clone()) {
                result.push(url.clone());
            }
        }

        if !should_skip_bootstrap {
            let default_bootstrap: Vec<String> = DEFAULT_BOOTSTRAP_RELAY_URLS
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            let bootstrap = self
                .bootstrap_relay_urls
                .as_deref()
                .unwrap_or(&default_bootstrap);
            for url in bootstrap {
                if seen.insert(url.clone()) {
                    result.push(url.clone());
                }
            }
        }

        result
    }

    /// Publish relay list (kind 10002).
    ///
    /// No-op if `should_publish_relay_list` is false or no relay URLs are available.
    /// Publishes to the merged advertised + bootstrap relay set.
    #[cfg(test)]
    pub(crate) async fn publish_relay_list(&self) -> Result<()> {
        if !self.should_publish_relay_list {
            return Ok(());
        }
        let urls = self.get_advertised_relay_urls();
        if urls.is_empty() {
            tracing::warn!(target: LOG_TARGET, "No relay URLs to publish relay list");
            return Ok(());
        }
        let tags: Vec<Tag> = urls
            .iter()
            .map(|url| Tag::custom(TagKind::Custom(tags::RELAY.into()), vec![url.clone()]))
            .collect();
        let builder = EventBuilder::new(Kind::Custom(RELAY_LIST_METADATA_KIND), "").tags(tags);
        match self.publish_to_discoverability_relays(builder).await {
            Ok(id) => tracing::info!(
                target: LOG_TARGET,
                event_id = %id,
                "Published relay list (kind 10002)"
            ),
            Err(e) => tracing::warn!(
                target: LOG_TARGET,
                error = %e,
                "Failed to publish relay list"
            ),
        }
        Ok(())
    }

    /// Publish profile metadata (kind 0).
    ///
    /// No-op if `profile_metadata` is not configured.
    /// Publishes to the merged advertised + bootstrap relay set.
    #[cfg(test)]
    pub(crate) async fn publish_profile_metadata(&self) -> Result<()> {
        let metadata = match &self.profile_metadata {
            Some(m) => m,
            None => return Ok(()),
        };
        let content = serde_json::to_string(metadata)?;
        let builder = EventBuilder::new(Kind::Custom(0), content);
        match self.publish_to_discoverability_relays(builder).await {
            Ok(id) => tracing::info!(
                target: LOG_TARGET,
                event_id = %id,
                "Published profile metadata (kind 0)"
            ),
            Err(e) => tracing::warn!(
                target: LOG_TARGET,
                error = %e,
                "Failed to publish profile metadata"
            ),
        }
        Ok(())
    }

    /// Publish an event to the discoverability relay set.
    ///
    /// Uses `get_discoverability_publish_relay_urls()` for targeted publication
    /// when bootstrap relays are configured. Falls back to pool-wide publish
    /// when the merged set is empty.
    #[cfg(test)]
    async fn publish_to_discoverability_relays(&self, builder: EventBuilder) -> Result<EventId> {
        let urls = self.get_discoverability_publish_relay_urls();
        if urls.is_empty() {
            self.relay_pool.publish(builder).await
        } else {
            self.relay_pool.publish_to(&urls, builder).await
        }
    }

    /// Spawn a task to publish profile metadata and relay list.
    ///
    /// Unconditional — guards live inside the individual publish methods.
    /// Event-building logic mirrors `publish_relay_list()` and `publish_profile_metadata()`.
    /// Duplication is intentional — `&self` can't be moved into a spawned task in Rust.
    #[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
    pub(crate) fn spawn_publish_discoverability(&self) -> tokio::task::JoinHandle<()> {
        let relay_pool = Arc::clone(&self.relay_pool);
        let target_urls = self.get_discoverability_publish_relay_urls();

        // Build events before spawning (borrows self) to avoid sending &self
        let profile_event = self.profile_metadata.as_ref().and_then(|metadata| {
            serde_json::to_string(metadata)
                .ok()
                .map(|content| EventBuilder::new(Kind::Custom(0), content))
        });

        let relay_list_event = if self.should_publish_relay_list {
            let urls = self.get_advertised_relay_urls();
            if urls.is_empty() {
                None
            } else {
                let tags: Vec<Tag> = urls
                    .iter()
                    .map(|url| Tag::custom(TagKind::Custom(tags::RELAY.into()), vec![url.clone()]))
                    .collect();
                Some(EventBuilder::new(Kind::Custom(RELAY_LIST_METADATA_KIND), "").tags(tags))
            }
        } else {
            None
        };

        tokio::spawn(async move {
            if let Some(builder) = profile_event {
                let result = if target_urls.is_empty() {
                    relay_pool.publish(builder).await
                } else {
                    relay_pool.publish_to(&target_urls, builder).await
                };
                match result {
                    Ok(id) => tracing::info!(
                        target: LOG_TARGET,
                        event_id = %id,
                        "Published profile metadata (kind 0)"
                    ),
                    Err(e) => tracing::warn!(
                        target: LOG_TARGET,
                        error = %e,
                        "Failed to publish profile metadata"
                    ),
                }
            }
            if let Some(builder) = relay_list_event {
                let result = if target_urls.is_empty() {
                    relay_pool.publish(builder).await
                } else {
                    relay_pool.publish_to(&target_urls, builder).await
                };
                match result {
                    Ok(id) => tracing::info!(
                        target: LOG_TARGET,
                        event_id = %id,
                        "Published relay list (kind 10002)"
                    ),
                    Err(e) => tracing::warn!(
                        target: LOG_TARGET,
                        error = %e,
                        "Failed to publish relay list"
                    ),
                }
            }
            tracing::info!(
                target: LOG_TARGET,
                "Discoverability event publishing complete"
            );
        })
    }

    // ── Event loop support ─────────────────────────────────────────

    /// Snapshot the tag state needed by the event loop.
    ///
    /// Returns a lightweight, cloneable copy of the fields the event loop
    /// needs for `append_common_response_tags` without holding a reference
    /// to the full manager.
    pub fn common_tags_snapshot(&self) -> CommonTagsSnapshot {
        CommonTagsSnapshot {
            server_info: self.server_info.clone(),
            extra_common_tags: self.extra_common_tags.clone(),
            encryption_mode: self.encryption_mode,
            gift_wrap_mode: self.gift_wrap_mode,
        }
    }

    /// Drop the dispatch channel clone so `close()` can fully shut down the
    /// message channel.
    pub(crate) fn shutdown(&mut self) {
        self.dispatch_fn.take();
    }

    // ── Auto-publish orchestration ────────────────────────────────

    /// Handle a response to a synthetic announcement request.
    ///
    /// Schema-matches the result to determine which event kind to publish:
    /// `InitializeResult` → 11316, `ListToolsResult` → 11317,
    /// `ListResourcesResult` → 11318, `ListResourceTemplatesResult` → 11319,
    /// `ListPromptsResult` → 11320.
    ///
    /// On `InitializeResult`, dispatches `notifications/initialized` via
    /// `dispatch_fn` **before** signaling `init_notify` — this ordering is
    /// critical so the notification enters the worker queue before any
    /// capability-list requests.
    #[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
    pub(crate) async fn handle_announcement_response(
        &self,
        response: JsonRpcMessage,
    ) -> Result<()> {
        let result = match &response {
            JsonRpcMessage::Response(resp) => &resp.result,
            JsonRpcMessage::ErrorResponse(resp) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    error_code = resp.error.code,
                    error_message = %resp.error.message,
                    "Announcement request returned error, skipping publish"
                );
                // If init hasn't completed yet, signal so publish_public_announcements
                // doesn't hang waiting for the Notify.
                let mut flag = self.initialized.lock().unwrap_or_else(|e| e.into_inner());
                if !*flag {
                    *flag = true;
                    drop(flag);
                    self.init_notify.notify_one();
                }
                return Ok(());
            }
            _ => return Ok(()),
        };

        // Determine event kind via the schema-to-kind mapping table.
        let kind = ANNOUNCEMENT_MAPPINGS
            .iter()
            .find(|m| (m.matches)(result))
            .map(|m| m.kind);

        if kind.is_none() {
            tracing::warn!(
                target: LOG_TARGET,
                "Announcement response has unrecognized schema, skipping publish"
            );
        }

        if let Some(kind) = kind {
            let content = serde_json::to_string(result)?;
            let tags = self.get_announcement_tags(kind);
            let builder = EventBuilder::new(Kind::Custom(kind), content).tags(tags);
            match self.relay_pool.publish(builder).await {
                Ok(id) => tracing::info!(
                    target: LOG_TARGET,
                    event_id = %id,
                    kind,
                    "Published announcement event"
                ),
                Err(e) => tracing::warn!(
                    target: LOG_TARGET,
                    error = %e,
                    kind,
                    "Failed to publish announcement event"
                ),
            }

            // For InitializeResult: dispatch notifications/initialized and signal Notify.
            if kind == SERVER_ANNOUNCEMENT_KIND {
                // Critical ordering: dispatch notifications/initialized FIRST so it
                // enters the worker queue before capability-list requests.
                if let Some(ref tx) = self.dispatch_fn {
                    let _ = tx.send(IncomingRequest {
                        message: JsonRpcMessage::Notification(JsonRpcNotification {
                            jsonrpc: "2.0".to_string(),
                            method: NOTIFICATIONS_INITIALIZED_METHOD.to_string(),
                            params: None,
                        }),
                        client_pubkey: ANNOUNCEMENT_REQUEST_ID.to_string(),
                        event_id: ANNOUNCEMENT_REQUEST_ID.to_string(),
                        is_encrypted: false,
                    });
                }

                // THEN signal the Notify — publish_public_announcements will dispatch
                // capability-list requests after this, ensuring they arrive after the
                // initialized notification in the worker queue.
                let mut flag = self.initialized.lock().unwrap_or_else(|e| e.into_inner());
                *flag = true;
                drop(flag);
                self.init_notify.notify_one();
            }
        }

        Ok(())
    }

    /// Spawn the auto-publish orchestration task.
    ///
    /// Returns a `JoinHandle` that the caller should track for cleanup.
    /// The task dispatches a synthetic `initialize` request, waits for the
    /// response, then dispatches capability-list requests.
    #[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
    pub(crate) fn spawn_publish_public_announcements(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let dispatch_fn = self
            .dispatch_fn
            .clone()
            .expect("dispatch_fn must be set before spawning announcements");
        let init_notify = Arc::clone(&self.init_notify);
        tokio::spawn(publish_public_announcements(
            dispatch_fn,
            init_notify,
            cancel,
        ))
    }
}

/// Auto-publish orchestration: dispatches synthetic requests and waits for init.
///
/// Standalone async function (not a method) so it can be moved into a spawned task
/// without borrowing the `AnnouncementManager`.
#[cfg_attr(not(feature = "rmcp"), allow(dead_code))]
async fn publish_public_announcements(
    dispatch_fn: tokio::sync::mpsc::UnboundedSender<IncomingRequest>,
    init_notify: Arc<Notify>,
    cancel: tokio_util::sync::CancellationToken,
) {
    tracing::info!(target: LOG_TARGET, "Starting auto-publish of server announcements");

    // Dispatch synthetic initialize request.
    let init_request = JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
        method: INITIALIZE_METHOD.to_string(),
        params: Some(serde_json::json!({
            "protocolVersion": crate::core::constants::mcp_protocol_version(),
            "capabilities": {},
            "clientInfo": {
                "name": "contextvm-announcement-client",
                "version": "0.1.0"
            }
        })),
    });
    if dispatch_fn
        .send(IncomingRequest {
            message: init_request,
            client_pubkey: ANNOUNCEMENT_REQUEST_ID.to_string(),
            event_id: ANNOUNCEMENT_REQUEST_ID.to_string(),
            is_encrypted: false,
        })
        .is_err()
    {
        tracing::warn!(
            target: LOG_TARGET,
            "Transport channel closed before init request could be sent"
        );
        return;
    }

    // Wait for handle_announcement_response to signal completion of the init
    // response, with cancellation support so close() isn't blocked.
    tokio::select! {
        _ = cancel.cancelled() => {
            tracing::info!(target: LOG_TARGET, "Announcement publish cancelled during init wait");
            return;
        }
        result = tokio::time::timeout(ANNOUNCEMENT_INIT_TIMEOUT, init_notify.notified()) => {
            match result {
                Ok(()) => tracing::info!(
                    target: LOG_TARGET,
                    "Announcement init complete, dispatching capability list requests"
                ),
                Err(_) => tracing::warn!(
                    target: LOG_TARGET,
                    timeout_secs = ANNOUNCEMENT_INIT_TIMEOUT.as_secs(),
                    "Announcement init timed out, proceeding with capability list requests"
                ),
            }
        }
    }

    // Dispatch all four capability-list requests at once (no per-request await).
    for method in &[
        "tools/list",
        "resources/list",
        "resources/templates/list",
        "prompts/list",
    ] {
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            method: method.to_string(),
            params: None,
        });
        let _ = dispatch_fn.send(IncomingRequest {
            message: request,
            client_pubkey: ANNOUNCEMENT_REQUEST_ID.to_string(),
            event_id: ANNOUNCEMENT_REQUEST_ID.to_string(),
            is_encrypted: false,
        });
    }

    tracing::info!(
        target: LOG_TARGET,
        "Dispatched all announcement capability list requests"
    );
}

/// Cloneable snapshot of tag-building state for the event loop.
///
/// Passed into the static `event_loop` function so it can append discovery
/// tags on first-response without holding a reference to the full manager.
#[derive(Clone)]
pub(crate) struct CommonTagsSnapshot {
    /// Server metadata for name tag.
    pub server_info: Option<ServerInfo>,
    /// User-provided extra common tags.
    pub extra_common_tags: Vec<Tag>,
    /// Encryption mode for capability tag decisions.
    pub encryption_mode: EncryptionMode,
    /// Gift-wrap mode for ephemeral tag decisions.
    pub gift_wrap_mode: GiftWrapMode,
}

impl CommonTagsSnapshot {
    /// Append server capability discovery tags to the given tag vec.
    ///
    /// Used in the event loop for first-response tags on announced-server
    /// unauthorized error responses.
    pub fn append_common_response_tags(&self, tags: &mut Vec<Tag>) {
        if self.encryption_mode != EncryptionMode::Disabled {
            tags.push(Tag::custom(
                TagKind::Custom(tags::SUPPORT_ENCRYPTION.into()),
                Vec::<String>::new(),
            ));
            if self.gift_wrap_mode.supports_ephemeral() {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::SUPPORT_ENCRYPTION_EPHEMERAL.into()),
                    Vec::<String>::new(),
                ));
            }
        }
        if let Some(ref info) = self.server_info {
            if let Some(ref name) = info.name {
                tags.push(Tag::custom(
                    TagKind::Custom(tags::NAME.into()),
                    vec![name.clone()],
                ));
            }
        }
        tags.extend(self.extra_common_tags.iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager(
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
        server_info: Option<ServerInfo>,
    ) -> AnnouncementManager {
        // Tests only exercise tag building; relay pool and dispatch channel are unused.
        use crate::relay::mock::MockRelayPool;
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(MockRelayPool::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AnnouncementManager::new(
            pool,
            server_info,
            encryption_mode,
            gift_wrap_mode,
            tx,
            Vec::new(),
            None,
            None,
            true,
            None,
        )
    }

    // ── 1. Server info tags ────────────────────────────────────────

    /// Helper: extract the first element (tag name) from a Tag.
    fn tag_name(tag: &Tag) -> String {
        tag.clone().to_vec().first().cloned().unwrap_or_default()
    }

    #[test]
    fn server_info_tags_all_fields() {
        let info = ServerInfo {
            name: Some("Test".into()),
            about: Some("A test server".into()),
            website: Some("https://example.com".into()),
            picture: Some("https://example.com/pic.png".into()),
            ..Default::default()
        };
        let mgr = make_manager(EncryptionMode::Disabled, GiftWrapMode::Optional, Some(info));
        let tags = mgr.get_server_info_tags();
        assert_eq!(tags.len(), 4);
        let names: Vec<String> = tags.iter().map(tag_name).collect();
        assert!(names.contains(&"name".to_string()));
        assert!(names.contains(&"about".to_string()));
        assert!(names.contains(&"website".to_string()));
        assert!(names.contains(&"picture".to_string()));
    }

    #[test]
    fn server_info_tags_partial() {
        let info = ServerInfo {
            name: Some("OnlyName".into()),
            ..Default::default()
        };
        let mgr = make_manager(EncryptionMode::Disabled, GiftWrapMode::Optional, Some(info));
        let tags = mgr.get_server_info_tags();
        assert_eq!(tags.len(), 1);
    }

    // ── 3–6. Capability tags ───────────────────────────────────────

    #[test]
    fn capability_tags_encryption_enabled() {
        let mgr = make_manager(EncryptionMode::Optional, GiftWrapMode::Persistent, None);
        let tags = mgr.get_capability_tags();
        let names: Vec<String> = tags.iter().map(tag_name).collect();
        assert!(names.contains(&tags::SUPPORT_ENCRYPTION.to_string()));
    }

    #[test]
    fn capability_tags_ephemeral_enabled() {
        let mgr = make_manager(EncryptionMode::Optional, GiftWrapMode::Optional, None);
        let tags = mgr.get_capability_tags();
        let names: Vec<String> = tags.iter().map(tag_name).collect();
        assert!(names.contains(&tags::SUPPORT_ENCRYPTION_EPHEMERAL.to_string()));
    }

    #[test]
    fn capability_tags_ephemeral_excluded() {
        let mgr = make_manager(EncryptionMode::Optional, GiftWrapMode::Persistent, None);
        let tags = mgr.get_capability_tags();
        let names: Vec<String> = tags.iter().map(tag_name).collect();
        assert!(
            !names.contains(&tags::SUPPORT_ENCRYPTION_EPHEMERAL.to_string()),
            "Persistent mode should not include ephemeral tag"
        );
    }

    #[test]
    fn capability_tags_encryption_disabled() {
        let mgr = make_manager(EncryptionMode::Disabled, GiftWrapMode::Optional, None);
        let tags = mgr.get_capability_tags();
        assert!(
            tags.is_empty(),
            "Disabled encryption should produce no capability tags"
        );
    }

    // ── 7. Caching ─────────────────────────────────────────────────

    #[test]
    fn common_tags_cached() {
        let info = ServerInfo {
            name: Some("Cache".into()),
            ..Default::default()
        };
        let mgr = make_manager(EncryptionMode::Optional, GiftWrapMode::Optional, Some(info));
        let first = mgr.get_common_tags();
        let second = mgr.get_common_tags();
        assert_eq!(first.len(), second.len());
        // Verify cache is populated
        let cache = mgr.cached_common_tags.lock().unwrap();
        assert!(
            cache.is_some(),
            "Cache should be populated after get_common_tags"
        );
    }

    // ── 8. Cache invalidation ──────────────────────────────────────

    #[test]
    fn set_extra_common_tags_invalidates_cache() {
        let mgr_info = ServerInfo {
            name: Some("Extra".into()),
            ..Default::default()
        };
        let mut mgr = make_manager(
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            Some(mgr_info),
        );

        // Populate cache
        let before = mgr.get_common_tags();
        assert!(!before.is_empty());

        // Set extra tags — should invalidate and include new tags
        let extra = vec![Tag::custom(
            TagKind::Custom("pmi".into()),
            vec!["lightning".to_string()],
        )];
        mgr.set_extra_common_tags(extra);
        let after = mgr.get_common_tags();
        assert_eq!(after.len(), before.len() + 1);
    }

    // ── 9. Pricing separate from common ────────────────────────────

    #[test]
    fn pricing_tags_separate_from_common() {
        let mut mgr = make_manager(EncryptionMode::Optional, GiftWrapMode::Optional, None);
        mgr.set_pricing_tags(vec![Tag::custom(
            TagKind::Custom(tags::CAPABILITY.into()),
            vec![
                "tool:echo".to_string(),
                "100".to_string(),
                "sats".to_string(),
            ],
        )]);
        let common = mgr.get_common_tags();
        let names: Vec<String> = common.iter().map(tag_name).collect();
        assert!(
            !names.contains(&tags::CAPABILITY.to_string()),
            "get_common_tags() should not include pricing tags"
        );
    }

    // ── 10. Announcement tags by kind ──────────────────────────────

    #[test]
    fn announcement_tags_kind_11316_includes_pricing() {
        let info = ServerInfo {
            name: Some("Ann".into()),
            ..Default::default()
        };
        let mut mgr = make_manager(EncryptionMode::Optional, GiftWrapMode::Optional, Some(info));
        mgr.set_pricing_tags(vec![Tag::custom(
            TagKind::Custom(tags::CAPABILITY.into()),
            vec![
                "tool:echo".to_string(),
                "100".to_string(),
                "sats".to_string(),
            ],
        )]);

        let ann_tags = mgr.get_announcement_tags(SERVER_ANNOUNCEMENT_KIND);
        let ann_names: Vec<String> = ann_tags.iter().map(tag_name).collect();
        assert!(
            ann_names.contains(&tags::CAPABILITY.to_string()),
            "Kind 11316 should include pricing tags"
        );

        let tools_tags = mgr.get_announcement_tags(TOOLS_LIST_KIND);
        let tools_names: Vec<String> = tools_tags.iter().map(tag_name).collect();
        assert!(
            !tools_names.contains(&"name".to_string()),
            "Kind 11317 should NOT include common tags, only pricing"
        );
        assert!(
            tools_names.contains(&tags::CAPABILITY.to_string()),
            "Kind 11317 should include pricing tags"
        );
    }

    // ── 11. Auto-publish: handle_announcement_response ───────────

    fn make_manager_with_pool(
        server_info: Option<ServerInfo>,
    ) -> (
        AnnouncementManager,
        Arc<crate::relay::mock::MockRelayPool>,
        tokio::sync::mpsc::UnboundedReceiver<IncomingRequest>,
    ) {
        use crate::relay::mock::MockRelayPool;
        let pool = Arc::new(MockRelayPool::new());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = AnnouncementManager::new(
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
            server_info,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            tx,
            Vec::new(),
            None,
            None,
            true,
            None,
        );
        (mgr, pool, rx)
    }

    #[tokio::test]
    async fn handle_announcement_response_publishes_init_result() {
        let (mgr, pool, mut rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": { "name": "test-server", "version": "0.1.0" }
            }),
        });

        mgr.handle_announcement_response(response).await.unwrap();

        // Verify kind 11316 event published
        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Custom(SERVER_ANNOUNCEMENT_KIND));

        // Verify notifications/initialized dispatched
        let notif = rx
            .try_recv()
            .expect("should dispatch notifications/initialized");
        assert_eq!(
            notif.message.method(),
            Some(NOTIFICATIONS_INITIALIZED_METHOD)
        );
        assert_eq!(notif.client_pubkey, ANNOUNCEMENT_REQUEST_ID);

        // Verify initialized flag set
        assert!(*mgr.initialized.lock().unwrap());
    }

    #[tokio::test]
    async fn handle_announcement_response_publishes_tools_list() {
        let (mgr, pool, _rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({
                "tools": [{ "name": "echo", "description": "Echo tool" }]
            }),
        });

        mgr.handle_announcement_response(response).await.unwrap();

        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Custom(TOOLS_LIST_KIND));
    }

    #[tokio::test]
    async fn handle_announcement_response_publishes_resources_list() {
        let (mgr, pool, _rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({ "resources": [] }),
        });

        mgr.handle_announcement_response(response).await.unwrap();

        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Custom(RESOURCES_LIST_KIND));
    }

    #[tokio::test]
    async fn handle_announcement_response_publishes_resource_templates_list() {
        let (mgr, pool, _rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({ "resourceTemplates": [] }),
        });

        mgr.handle_announcement_response(response).await.unwrap();

        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Custom(RESOURCETEMPLATES_LIST_KIND));
    }

    #[tokio::test]
    async fn handle_announcement_response_publishes_prompts_list() {
        let (mgr, pool, _rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({ "prompts": [] }),
        });

        mgr.handle_announcement_response(response).await.unwrap();

        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Custom(PROMPTS_LIST_KIND));
    }

    #[tokio::test]
    async fn handle_announcement_response_error_signals_notify_without_publishing() {
        let (mgr, pool, _rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            error: JsonRpcError {
                code: -32600,
                message: "test error".to_string(),
                data: None,
            },
        });

        mgr.handle_announcement_response(response).await.unwrap();

        // No events published
        assert!(pool.stored_events().await.is_empty());
        // But initialized flag is set (to unblock publish_public_announcements)
        assert!(*mgr.initialized.lock().unwrap());
    }

    #[tokio::test]
    async fn handle_announcement_response_unknown_schema_no_publish() {
        let (mgr, pool, _rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({ "unknown": "data" }),
        });

        mgr.handle_announcement_response(response).await.unwrap();
        assert!(pool.stored_events().await.is_empty());
    }

    // ── 12. Auto-publish: publish_public_announcements ───────────

    #[tokio::test]
    async fn publish_public_announcements_dispatches_init_then_capability_lists() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IncomingRequest>();
        let init_notify = Arc::new(Notify::new());
        let cancel = tokio_util::sync::CancellationToken::new();

        let init_notify_clone = Arc::clone(&init_notify);
        let handle = tokio::spawn(publish_public_announcements(tx, init_notify_clone, cancel));

        // First message should be the synthetic initialize request.
        let init_msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive init request within 1s")
            .expect("channel should not be closed");
        assert_eq!(init_msg.message.method(), Some(INITIALIZE_METHOD));
        assert_eq!(init_msg.client_pubkey, ANNOUNCEMENT_REQUEST_ID);
        assert_eq!(init_msg.event_id, ANNOUNCEMENT_REQUEST_ID);
        assert!(!init_msg.is_encrypted);

        // Signal init complete — the task should then dispatch capability lists.
        init_notify.notify_one();

        // Should receive 4 capability list requests in order.
        let expected_methods = [
            "tools/list",
            "resources/list",
            "resources/templates/list",
            "prompts/list",
        ];
        for expected_method in &expected_methods {
            let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("should receive capability request within 1s")
                .expect("channel should not be closed");
            assert_eq!(msg.message.method(), Some(*expected_method));
            assert_eq!(msg.client_pubkey, ANNOUNCEMENT_REQUEST_ID);
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn handle_init_result_dispatches_notification_before_notify_signal() {
        let (mgr, _pool, mut rx) = make_manager_with_pool(None);

        // Clone the Notify so we can also wait on it from the test.
        let init_notify = Arc::clone(&mgr.init_notify);
        let notified = init_notify.notified();

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "serverInfo": { "name": "test" }
            }),
        });

        mgr.handle_announcement_response(response).await.unwrap();

        // The Notify should have been signaled.
        tokio::time::timeout(Duration::from_millis(100), notified)
            .await
            .expect("init_notify should have been signaled");

        // And the notifications/initialized message should already be in the
        // channel (dispatched BEFORE the Notify signal).
        let notif = rx
            .try_recv()
            .expect("notification should be queued before Notify");
        assert_eq!(
            notif.message.method(),
            Some(NOTIFICATIONS_INITIALIZED_METHOD)
        );
    }

    // ── 13. Relay list + profile metadata (CEP-6) ──────────────────

    fn make_manager_with_discoverability(
        relay_urls: Vec<String>,
        relay_list_urls: Option<Vec<String>>,
        bootstrap_relay_urls: Option<Vec<String>>,
        publish_relay_list: bool,
        profile_metadata: Option<ProfileMetadata>,
    ) -> (AnnouncementManager, Arc<crate::relay::mock::MockRelayPool>) {
        use crate::relay::mock::MockRelayPool;
        let pool = Arc::new(MockRelayPool::new());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = AnnouncementManager::new(
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
            None,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            tx,
            relay_urls,
            relay_list_urls,
            bootstrap_relay_urls,
            publish_relay_list,
            profile_metadata,
        );
        (mgr, pool)
    }

    #[test]
    fn is_local_relay_url_detects_localhost() {
        assert!(is_local_relay_url("ws://localhost:7777"));
        assert!(is_local_relay_url("wss://localhost"));
        assert!(is_local_relay_url("ws://127.0.0.1:8080"));
        assert!(is_local_relay_url("ws://0.0.0.0:9999"));
        assert!(is_local_relay_url("ws://[::1]:7777"));
    }

    #[test]
    fn is_local_relay_url_rejects_remote() {
        assert!(!is_local_relay_url("wss://relay.damus.io"));
        assert!(!is_local_relay_url("wss://relay.example.com"));
        assert!(!is_local_relay_url("ws://10.0.0.1:7777"));
    }

    #[test]
    fn get_advertised_relay_urls_uses_relay_list_when_set() {
        let (mgr, _pool) = make_manager_with_discoverability(
            vec!["wss://connected.relay".into()],
            Some(vec!["wss://advertised.relay".into()]),
            None,
            true,
            None,
        );
        assert_eq!(mgr.get_advertised_relay_urls(), &["wss://advertised.relay"]);
    }

    #[test]
    fn get_advertised_relay_urls_falls_back_to_relay_urls() {
        let (mgr, _pool) = make_manager_with_discoverability(
            vec!["wss://connected.relay".into()],
            None,
            None,
            true,
            None,
        );
        assert_eq!(mgr.get_advertised_relay_urls(), &["wss://connected.relay"]);
    }

    #[test]
    fn get_discoverability_urls_merges_bootstrap() {
        let (mgr, _pool) = make_manager_with_discoverability(
            vec!["wss://my.relay".into()],
            None,
            None,
            true,
            None,
        );
        let urls = mgr.get_discoverability_publish_relay_urls();
        assert!(urls.contains(&"wss://my.relay".to_string()));
        // Should include default bootstrap relays
        assert!(urls.len() > 1);
        assert!(urls.contains(&DEFAULT_BOOTSTRAP_RELAY_URLS[0].to_string()));
    }

    #[test]
    fn get_discoverability_urls_skips_bootstrap_for_local_only() {
        let (mgr, _pool) = make_manager_with_discoverability(
            vec!["ws://127.0.0.1:7777".into()],
            None,
            None,
            true,
            None,
        );
        let urls = mgr.get_discoverability_publish_relay_urls();
        assert_eq!(urls, vec!["ws://127.0.0.1:7777"]);
    }

    #[test]
    fn get_discoverability_urls_keeps_explicit_bootstrap_even_for_local() {
        let (mgr, _pool) = make_manager_with_discoverability(
            vec!["ws://127.0.0.1:7777".into()],
            None,
            Some(vec!["wss://explicit-bootstrap.relay".into()]),
            true,
            None,
        );
        let urls = mgr.get_discoverability_publish_relay_urls();
        assert!(urls.contains(&"ws://127.0.0.1:7777".to_string()));
        assert!(urls.contains(&"wss://explicit-bootstrap.relay".to_string()));
    }

    #[test]
    fn get_discoverability_urls_deduplicates() {
        let (mgr, _pool) = make_manager_with_discoverability(
            vec!["wss://relay.damus.io".into()],
            None,
            None,
            true,
            None,
        );
        let urls = mgr.get_discoverability_publish_relay_urls();
        let damus_count = urls.iter().filter(|u| *u == "wss://relay.damus.io").count();
        assert_eq!(damus_count, 1, "should be deduplicated");
    }

    #[tokio::test]
    async fn publish_relay_list_event_shape() {
        let (mgr, pool) = make_manager_with_discoverability(
            vec![
                "wss://relay1.example.com".into(),
                "wss://relay2.example.com".into(),
            ],
            None,
            None,
            true,
            None,
        );
        mgr.publish_relay_list().await.unwrap();
        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Custom(RELAY_LIST_METADATA_KIND));
        assert_eq!(events[0].content, "");
        let tag_values: Vec<String> = events[0]
            .tags
            .iter()
            .filter(|t| (*t).clone().to_vec().first().map(|s| s.as_str()) == Some("r"))
            .filter_map(|t| (*t).clone().to_vec().get(1).cloned())
            .collect();
        assert_eq!(
            tag_values,
            vec!["wss://relay1.example.com", "wss://relay2.example.com"]
        );
    }

    #[tokio::test]
    async fn publish_relay_list_uses_relay_list_urls_override() {
        let (mgr, pool) = make_manager_with_discoverability(
            vec!["wss://connected.relay".into()],
            Some(vec!["wss://override.relay".into()]),
            None,
            true,
            None,
        );
        mgr.publish_relay_list().await.unwrap();
        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        let tag_values: Vec<String> = events[0]
            .tags
            .iter()
            .filter(|t| (*t).clone().to_vec().first().map(|s| s.as_str()) == Some("r"))
            .filter_map(|t| (*t).clone().to_vec().get(1).cloned())
            .collect();
        assert_eq!(tag_values, vec!["wss://override.relay"]);
    }

    #[tokio::test]
    async fn publish_relay_list_opt_out() {
        let (mgr, pool) = make_manager_with_discoverability(
            vec!["wss://relay.example.com".into()],
            None,
            None,
            false,
            None,
        );
        mgr.publish_relay_list().await.unwrap();
        assert!(
            pool.stored_events().await.is_empty(),
            "should not publish when disabled"
        );
    }

    #[tokio::test]
    async fn publish_relay_list_empty_urls_no_publish() {
        let (mgr, pool) = make_manager_with_discoverability(Vec::new(), None, None, true, None);
        mgr.publish_relay_list().await.unwrap();
        assert!(
            pool.stored_events().await.is_empty(),
            "should not publish with no URLs"
        );
    }

    #[tokio::test]
    async fn publish_profile_metadata_event_shape() {
        let metadata = ProfileMetadata::default()
            .with_name("Test Server")
            .with_about("A test MCP server");
        let (mgr, pool) =
            make_manager_with_discoverability(Vec::new(), None, None, true, Some(metadata));
        mgr.publish_profile_metadata().await.unwrap();
        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Custom(0));
        assert!(events[0].tags.is_empty());
        let parsed: ProfileMetadata = serde_json::from_str(&events[0].content).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("Test Server"));
        assert_eq!(parsed.about.as_deref(), Some("A test MCP server"));
    }

    #[tokio::test]
    async fn publish_profile_metadata_noop_when_unconfigured() {
        let (mgr, pool) = make_manager_with_discoverability(Vec::new(), None, None, true, None);
        mgr.publish_profile_metadata().await.unwrap();
        assert!(
            pool.stored_events().await.is_empty(),
            "should not publish without profile metadata"
        );
    }

    // ── 14. Integration tests + cleanup (CEP-6) ────────────────────

    #[test]
    fn announcement_mapping_table_covers_all_kinds() {
        let kinds: HashSet<u16> = ANNOUNCEMENT_MAPPINGS.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&SERVER_ANNOUNCEMENT_KIND));
        assert!(kinds.contains(&TOOLS_LIST_KIND));
        assert!(kinds.contains(&RESOURCES_LIST_KIND));
        assert!(kinds.contains(&RESOURCETEMPLATES_LIST_KIND));
        assert!(kinds.contains(&PROMPTS_LIST_KIND));
    }

    #[test]
    fn announcement_mapping_resource_templates_regression() {
        let result = serde_json::json!({ "resourceTemplates": [] });
        let kind = ANNOUNCEMENT_MAPPINGS
            .iter()
            .find(|m| (m.matches)(&result))
            .map(|m| m.kind);
        assert_eq!(kind, Some(RESOURCETEMPLATES_LIST_KIND));
    }

    #[test]
    fn announcement_mapping_rejects_null_field() {
        let result = serde_json::json!({ "tools": null });
        let kind = ANNOUNCEMENT_MAPPINGS
            .iter()
            .find(|m| (m.matches)(&result))
            .map(|m| m.kind);
        assert_eq!(kind, None, "null field should not match any schema");
    }

    #[test]
    fn announcement_mapping_rejects_wrong_type() {
        let result = serde_json::json!({ "tools": "not an array" });
        let kind = ANNOUNCEMENT_MAPPINGS
            .iter()
            .find(|m| (m.matches)(&result))
            .map(|m| m.kind);
        assert_eq!(kind, None, "string field should not match array schema");
    }

    #[tokio::test]
    async fn handle_announcement_response_rejects_malformed_payload() {
        let (mgr, pool, _rx) = make_manager_with_pool(None);

        let response = JsonRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
            result: serde_json::json!({ "tools": null }),
        });
        mgr.handle_announcement_response(response).await.unwrap();
        assert!(
            pool.stored_events().await.is_empty(),
            "malformed payload should not produce an event"
        );
    }

    #[tokio::test]
    async fn roundtrip_publish_discover_all_5_kinds() {
        let info = ServerInfo {
            name: Some("Roundtrip".into()),
            ..Default::default()
        };
        let (mgr, pool, _rx) = make_manager_with_pool(Some(info));

        let responses = vec![
            serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "Roundtrip", "version": "1.0"}
            }),
            serde_json::json!({"tools": [{"name": "echo"}]}),
            serde_json::json!({"resources": [{"name": "config"}]}),
            serde_json::json!({"resourceTemplates": [{"name": "tmpl"}]}),
            serde_json::json!({"prompts": [{"name": "greet"}]}),
        ];
        for result in responses {
            mgr.handle_announcement_response(JsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(ANNOUNCEMENT_REQUEST_ID),
                result,
            }))
            .await
            .unwrap();
        }

        // Discover via fetch_events for each kind
        let pubkey = pool.mock_public_key();
        for (kind, key) in [
            (SERVER_ANNOUNCEMENT_KIND, "protocolVersion"),
            (TOOLS_LIST_KIND, "tools"),
            (RESOURCES_LIST_KIND, "resources"),
            (RESOURCETEMPLATES_LIST_KIND, "resourceTemplates"),
            (PROMPTS_LIST_KIND, "prompts"),
        ] {
            let filter = Filter::new().kind(Kind::Custom(kind)).author(pubkey);
            let events = pool
                .fetch_events(filter, Duration::from_secs(1))
                .await
                .unwrap();
            assert_eq!(events.len(), 1, "kind {kind} should have exactly 1 event");
            let content: serde_json::Value = serde_json::from_str(&events[0].content).unwrap();
            assert!(
                content.get(key).is_some(),
                "kind {kind} content should contain '{key}'"
            );
        }
    }

    // ── 15. CEP-6 parity tests ──────────────────────────────────────

    #[tokio::test]
    async fn publish_profile_metadata_preserves_custom_fields() {
        let mut metadata = ProfileMetadata::default()
            .with_name("Full Server")
            .with_about("All fields present")
            .with_picture("https://example.com/pic.png")
            .with_banner("https://example.com/banner.png")
            .with_website("https://example.com")
            .with_nip05("server@example.com")
            .with_lud16("server@walletofsatoshi.com");
        metadata
            .extra
            .insert("custom_flag".into(), serde_json::json!(true));

        let (mgr, pool) =
            make_manager_with_discoverability(Vec::new(), None, None, true, Some(metadata));
        mgr.publish_profile_metadata().await.unwrap();

        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        let parsed: ProfileMetadata = serde_json::from_str(&events[0].content).unwrap();
        assert_eq!(parsed.name.as_deref(), Some("Full Server"));
        assert_eq!(parsed.about.as_deref(), Some("All fields present"));
        assert_eq!(
            parsed.picture.as_deref(),
            Some("https://example.com/pic.png")
        );
        assert_eq!(
            parsed.banner.as_deref(),
            Some("https://example.com/banner.png")
        );
        assert_eq!(parsed.website.as_deref(), Some("https://example.com"));
        assert_eq!(parsed.nip05.as_deref(), Some("server@example.com"));
        assert_eq!(parsed.lud16.as_deref(), Some("server@walletofsatoshi.com"));
        assert_eq!(
            parsed.extra.get("custom_flag"),
            Some(&serde_json::json!(true)),
            "custom field should survive publish round-trip"
        );
    }

    #[tokio::test]
    async fn publish_profile_metadata_publish_error_does_not_panic() {
        use crate::relay::mock::MockRelayPool;
        use std::sync::atomic::{AtomicBool, Ordering};

        /// A relay pool that fails on publish when the flag is set.
        struct FailingPool {
            inner: MockRelayPool,
            should_fail: AtomicBool,
        }

        #[async_trait::async_trait]
        impl RelayPoolTrait for FailingPool {
            async fn connect(&self, urls: &[String]) -> Result<()> {
                self.inner.connect(urls).await
            }
            async fn disconnect(&self) -> Result<()> {
                self.inner.disconnect().await
            }
            async fn publish_event(&self, event: &Event) -> Result<EventId> {
                self.inner.publish_event(event).await
            }
            async fn publish(&self, builder: EventBuilder) -> Result<EventId> {
                if self.should_fail.load(Ordering::SeqCst) {
                    return Err(Error::Transport("injected publish failure".into()));
                }
                self.inner.publish(builder).await
            }
            async fn sign(&self, builder: EventBuilder) -> Result<Event> {
                self.inner.sign(builder).await
            }
            async fn signer(&self) -> Result<Arc<dyn NostrSigner>> {
                self.inner.signer().await
            }
            fn notifications(&self) -> tokio::sync::broadcast::Receiver<RelayPoolNotification> {
                self.inner.notifications()
            }
            async fn public_key(&self) -> Result<PublicKey> {
                self.inner.public_key().await
            }
            async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
                self.inner.subscribe(filters).await
            }
            async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId> {
                if self.should_fail.load(Ordering::SeqCst) {
                    return Err(Error::Transport("injected publish failure".into()));
                }
                self.inner.publish_to(urls, builder).await
            }
            async fn fetch_events(&self, filter: Filter, timeout: Duration) -> Result<Vec<Event>> {
                self.inner.fetch_events(filter, timeout).await
            }
        }

        let pool: Arc<dyn RelayPoolTrait> = Arc::new(FailingPool {
            inner: MockRelayPool::new(),
            should_fail: AtomicBool::new(true),
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let metadata = ProfileMetadata::default().with_name("Err Server");
        let mgr = AnnouncementManager::new(
            pool,
            None,
            EncryptionMode::Disabled,
            GiftWrapMode::Optional,
            tx,
            Vec::new(),
            None,
            None,
            true,
            Some(metadata),
        );

        // publish_profile_metadata swallows the error and returns Ok(())
        let result = mgr.publish_profile_metadata().await;
        assert!(
            result.is_ok(),
            "publish_profile_metadata should swallow publish errors"
        );
    }

    #[tokio::test]
    async fn private_server_publishes_relay_list_but_not_announcements() {
        // A private server (is_announced_server: false at transport level) still
        // publishes relay list for discoverability. The AnnouncementManager doesn't
        // gate on is_announced_server — that's the transport's job — so calling
        // publish_relay_list() should work, while never calling announce() means
        // no kind 11316 events exist.
        let (mgr, pool) = make_manager_with_discoverability(
            vec!["wss://relay.example.com".into()],
            None,
            None,
            true,
            None,
        );

        // Publish relay list (kind 10002) — should succeed.
        mgr.publish_relay_list().await.unwrap();

        let events = pool.stored_events().await;
        let relay_list_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == Kind::Custom(RELAY_LIST_METADATA_KIND))
            .collect();
        assert_eq!(
            relay_list_events.len(),
            1,
            "relay list (kind 10002) should be published"
        );

        // No kind 11316 events — announce() was never called (private server).
        let announcement_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
            .collect();
        assert!(
            announcement_events.is_empty(),
            "private server should have no kind 11316 announcements"
        );
    }

    #[tokio::test]
    async fn relay_list_advertises_different_urls_than_bootstrap() {
        let (mgr, pool) = make_manager_with_discoverability(
            vec!["wss://connected.relay".into()],
            Some(vec![
                "wss://public1.relay".into(),
                "wss://public2.relay".into(),
            ]),
            Some(vec!["wss://bootstrap.relay".into()]),
            true,
            None,
        );
        mgr.publish_relay_list().await.unwrap();

        let events = pool.stored_events().await;
        assert_eq!(events.len(), 1);
        let tag_values: Vec<String> = events[0]
            .tags
            .iter()
            .filter(|t| (*t).clone().to_vec().first().map(|s| s.as_str()) == Some("r"))
            .filter_map(|t| (*t).clone().to_vec().get(1).cloned())
            .collect();
        assert_eq!(
            tag_values,
            vec!["wss://public1.relay", "wss://public2.relay"],
            "kind 10002 tags should contain only relay_list_urls"
        );
        assert!(
            !tag_values.contains(&"wss://bootstrap.relay".to_string()),
            "bootstrap URLs must not appear in kind 10002 tags"
        );
    }

    #[tokio::test]
    async fn profile_metadata_published_regardless_of_announced_server() {
        // Profile metadata (kind 0) is decoupled from is_announced_server.
        // The AnnouncementManager publishes it whenever configured, even if the
        // transport never calls announce().
        let metadata = ProfileMetadata::default()
            .with_name("Private Server")
            .with_about("Not publicly announced");
        let (mgr, pool) =
            make_manager_with_discoverability(Vec::new(), None, None, true, Some(metadata));

        // Publish only profile metadata — do NOT call announce().
        mgr.publish_profile_metadata().await.unwrap();

        let events = pool.stored_events().await;
        let profile_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == Kind::Custom(0))
            .collect();
        assert_eq!(
            profile_events.len(),
            1,
            "kind 0 should be published even without announcements"
        );

        let announcement_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind == Kind::Custom(SERVER_ANNOUNCEMENT_KIND))
            .collect();
        assert!(
            announcement_events.is_empty(),
            "no kind 11316 should exist when announce() not called"
        );
    }

    #[tokio::test]
    async fn delete_announcements_uses_e_tags() {
        let info = ServerInfo {
            name: Some("Del".into()),
            ..Default::default()
        };
        let (mgr, pool, _rx) = make_manager_with_pool(Some(info));

        // Publish an announcement (kind 11316)
        mgr.announce().await.unwrap();
        let published = pool.stored_events().await;
        let announcement_id = published[0].id;

        // Delete
        mgr.delete_announcements("going offline").await.unwrap();

        // Find deletion events (kind 5)
        let all_events = pool.stored_events().await;
        let deletion_events: Vec<_> = all_events
            .iter()
            .filter(|e| e.kind == Kind::Custom(5))
            .collect();
        assert!(!deletion_events.is_empty(), "should have deletion events");

        // Verify ["e", event_id] tags, not ["k", kind]
        let del = &deletion_events[0];
        let tags: Vec<Vec<String>> = del.tags.iter().map(|t| (*t).clone().to_vec()).collect();
        assert!(!tags.is_empty(), "deletion event should have tags");
        for tag in &tags {
            assert_eq!(tag[0], "e", "deletion tag should be 'e', not 'k'");
        }
        let ann_id_hex = announcement_id.to_hex();
        assert!(
            tags.iter()
                .any(|t| t.get(1).map(|s| s.as_str()) == Some(ann_id_hex.as_str())),
            "deletion should reference the published announcement event ID"
        );
        assert_eq!(del.content, "going offline");
        assert_eq!(
            deletion_events.len(),
            1,
            "only one kind was published so only one deletion event expected"
        );
    }
}
