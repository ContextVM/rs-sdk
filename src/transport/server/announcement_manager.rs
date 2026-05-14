//! Announcement tag management for the server transport.
//!
//! Encapsulates tag composition, caching, and publishing for CEP-6 server
//! announcements (kinds 11316–11320) and CEP-35 first-response discovery.

use std::sync::{Arc, Mutex};

use nostr_sdk::prelude::*;

use crate::core::constants::*;
use crate::core::error::{Error, Result};
use crate::core::types::*;
use crate::relay::RelayPoolTrait;

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
}

impl AnnouncementManager {
    /// Create a new announcement manager.
    pub fn new(
        relay_pool: Arc<dyn RelayPoolTrait>,
        server_info: Option<ServerInfo>,
        encryption_mode: EncryptionMode,
        gift_wrap_mode: GiftWrapMode,
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
    pub async fn delete_announcements(&self, reason: &str) -> Result<()> {
        for kind in UNENCRYPTED_KINDS {
            let builder = EventBuilder::new(Kind::Custom(5), reason).tag(Tag::custom(
                TagKind::Custom("k".into()),
                vec![kind.to_string()],
            ));
            self.relay_pool.publish(builder).await?;
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
        // Tests only exercise tag building; relay pool is unused.
        use crate::relay::mock::MockRelayPool;
        let pool: Arc<dyn RelayPoolTrait> = Arc::new(MockRelayPool::new());
        AnnouncementManager::new(pool, server_info, encryption_mode, gift_wrap_mode)
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
}
