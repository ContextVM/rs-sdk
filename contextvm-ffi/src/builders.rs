//! Helper functions for building SDK types from FFI configs.
//!
//! The SDK's public types are `#[non_exhaustive]`, so we can't construct
//! them with struct literals from outside the crate.  These helpers use
//! the builder pattern instead.

use std::time::Duration;

use crate::types::{c_str_array_to_vec, c_str_to_string, FfiClientConfig, FfiServerConfig};

pub struct ServerConfigParts {
    pub relay_urls: Vec<String>,
    pub encryption_mode: contextvm_sdk::EncryptionMode,
    pub gift_wrap_mode: contextvm_sdk::GiftWrapMode,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub server_picture: Option<String>,
    pub server_about: Option<String>,
    pub server_website: Option<String>,
    pub is_announced_server: bool,
    pub allowed_pubkeys: Vec<String>,
    pub session_timeout_secs: u64,
    pub cleanup_interval_secs: u64,
}

/// Build a `ServerInfo` from FFI C strings.
pub fn build_server_info(
    name: Option<String>,
    version: Option<String>,
    picture: Option<String>,
    about: Option<String>,
    website: Option<String>,
) -> Option<contextvm_sdk::ServerInfo> {
    if name.is_none()
        && version.is_none()
        && picture.is_none()
        && about.is_none()
        && website.is_none()
    {
        return None;
    }

    let mut info = contextvm_sdk::ServerInfo::default();
    if let Some(n) = name {
        info = info.with_name(n);
    }
    if let Some(v) = version {
        info = info.with_version(v);
    }
    if let Some(p) = picture {
        info = info.with_picture(p);
    }
    if let Some(a) = about {
        info = info.with_about(a);
    }
    if let Some(w) = website {
        info = info.with_website(w);
    }
    Some(info)
}

/// Extract fields from an FfiServerConfig and build an SDK server config.
pub fn build_sdk_server_config(
    config: &FfiServerConfig,
) -> contextvm_sdk::NostrServerTransportConfig {
    let relay_urls = c_str_array_to_vec(config.relay_urls, config.relay_url_count);
    let allowed = c_str_array_to_vec(config.allowed_pubkeys, config.allowed_pubkey_count);

    let server_info = build_server_info(
        c_str_to_string(config.server_name),
        c_str_to_string(config.server_version),
        c_str_to_string(config.server_picture),
        c_str_to_string(config.server_about),
        c_str_to_string(config.server_website),
    );

    let mut sdk_config = contextvm_sdk::NostrServerTransportConfig::default()
        .with_relay_urls(if relay_urls.is_empty() {
            vec!["wss://relay.damus.io".to_string()]
        } else {
            relay_urls
        })
        .with_encryption_mode(config.encryption_mode.into())
        .with_gift_wrap_mode(config.gift_wrap_mode.into())
        .with_announced_server(config.is_announced_server)
        .with_allowed_public_keys(allowed)
        .with_session_timeout(Duration::from_secs(config.session_timeout_secs.max(1)))
        .with_cleanup_interval(Duration::from_secs(config.cleanup_interval_secs.max(1)));

    if let Some(server_info) = server_info {
        sdk_config = sdk_config.with_server_info(server_info);
    }

    sdk_config
}

/// Extract fields from an FfiClientConfig and build an SDK client config.
pub fn build_sdk_client_config(
    config: &FfiClientConfig,
) -> Option<contextvm_sdk::NostrClientTransportConfig> {
    let server_pubkey = c_str_to_string(config.server_pubkey)?;
    let relay_urls = c_str_array_to_vec(config.relay_urls, config.relay_url_count);

    Some(
        contextvm_sdk::NostrClientTransportConfig::default()
            .with_relay_urls(relay_urls)
            .with_server_pubkey(server_pubkey)
            .with_encryption_mode(config.encryption_mode.into())
            .with_gift_wrap_mode(config.gift_wrap_mode.into())
            .with_stateless(config.is_stateless)
            .with_timeout(Duration::from_secs(config.timeout_secs.max(1))),
    )
}

/// Build a server config from UniFFI record fields.
pub fn build_sdk_server_config_from_fields(
    parts: ServerConfigParts,
) -> contextvm_sdk::NostrServerTransportConfig {
    let server_info = build_server_info(
        parts.server_name,
        parts.server_version,
        parts.server_picture,
        parts.server_about,
        parts.server_website,
    );

    let mut sdk_config = contextvm_sdk::NostrServerTransportConfig::default()
        .with_relay_urls(if parts.relay_urls.is_empty() {
            vec!["wss://relay.damus.io".to_string()]
        } else {
            parts.relay_urls
        })
        .with_encryption_mode(parts.encryption_mode)
        .with_gift_wrap_mode(parts.gift_wrap_mode)
        .with_announced_server(parts.is_announced_server)
        .with_allowed_public_keys(parts.allowed_pubkeys)
        .with_session_timeout(Duration::from_secs(parts.session_timeout_secs.max(1)))
        .with_cleanup_interval(Duration::from_secs(parts.cleanup_interval_secs.max(1)));

    if let Some(server_info) = server_info {
        sdk_config = sdk_config.with_server_info(server_info);
    }

    sdk_config
}

/// Build a client config from UniFFI record fields.
pub fn build_sdk_client_config_from_fields(
    relay_urls: Vec<String>,
    server_pubkey: String,
    encryption_mode: contextvm_sdk::EncryptionMode,
    gift_wrap_mode: contextvm_sdk::GiftWrapMode,
    is_stateless: bool,
    timeout_secs: u64,
) -> contextvm_sdk::NostrClientTransportConfig {
    contextvm_sdk::NostrClientTransportConfig::default()
        .with_relay_urls(relay_urls)
        .with_server_pubkey(server_pubkey)
        .with_encryption_mode(encryption_mode)
        .with_gift_wrap_mode(gift_wrap_mode)
        .with_stateless(is_stateless)
        .with_timeout(Duration::from_secs(timeout_secs.max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_server_info_matches_sdk_default() {
        assert!(build_server_info(None, None, None, None, None).is_none());

        let config = build_sdk_server_config_from_fields(ServerConfigParts {
            relay_urls: vec!["wss://relay.example.com".to_string()],
            encryption_mode: contextvm_sdk::EncryptionMode::Optional,
            gift_wrap_mode: contextvm_sdk::GiftWrapMode::Optional,
            server_name: None,
            server_version: None,
            server_picture: None,
            server_about: None,
            server_website: None,
            is_announced_server: false,
            allowed_pubkeys: vec![],
            session_timeout_secs: 300,
            cleanup_interval_secs: 60,
        });

        assert!(config.server_info.is_none());
    }

    #[test]
    fn server_info_preserves_sdk_fields() {
        let info = build_server_info(
            Some("name".to_string()),
            Some("1.2.3".to_string()),
            Some("https://example.com/pic.png".to_string()),
            Some("about".to_string()),
            Some("https://example.com".to_string()),
        )
        .expect("server info should be present when any field is supplied");

        assert_eq!(info.name.as_deref(), Some("name"));
        assert_eq!(info.version.as_deref(), Some("1.2.3"));
        assert_eq!(info.picture.as_deref(), Some("https://example.com/pic.png"));
        assert_eq!(info.about.as_deref(), Some("about"));
        assert_eq!(info.website.as_deref(), Some("https://example.com"));
    }
}
