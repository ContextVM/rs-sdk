//! Helper functions for building SDK types from FFI configs.
//!
//! The SDK's public types are `#[non_exhaustive]`, so we can't construct
//! them with struct literals from outside the crate.  These helpers use
//! the builder pattern instead.

use std::time::Duration;

use crate::error::{ErrorCode, FfiError};
use crate::types::{
    c_str_array_to_vec_checked, c_str_to_string_checked, ffi_encryption_mode_to_sdk,
    ffi_gift_wrap_mode_to_sdk, optional_c_str_to_string_checked, FfiCapabilityExclusion,
    FfiClientConfig, FfiServerConfig,
};

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
    pub excluded_capabilities: Vec<CapabilityExclusionParts>,
    pub max_sessions: usize,
    pub request_timeout_secs: u64,
    pub relay_list_urls: Vec<String>,
    pub bootstrap_relay_urls: Vec<String>,
    pub publish_relay_list: bool,
    pub profile_metadata_json: Option<String>,
}

pub struct ClientConfigParts {
    pub relay_urls: Vec<String>,
    pub server_pubkey: String,
    pub encryption_mode: contextvm_sdk::EncryptionMode,
    pub gift_wrap_mode: contextvm_sdk::GiftWrapMode,
    pub is_stateless: bool,
    pub timeout_secs: u64,
    pub discovery_relay_urls: Vec<String>,
    pub fallback_operational_relay_urls: Vec<String>,
}

#[derive(Clone)]
pub struct CapabilityExclusionParts {
    pub method: String,
    pub name: Option<String>,
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
) -> Result<contextvm_sdk::NostrServerTransportConfig, FfiError> {
    let relay_urls =
        c_str_array_to_vec_checked(config.relay_urls, config.relay_url_count, "relay_urls")?;
    let allowed = c_str_array_to_vec_checked(
        config.allowed_pubkeys,
        config.allowed_pubkey_count,
        "allowed_pubkeys",
    )?;
    let excluded = ffi_capability_exclusions_to_parts(
        config.excluded_capabilities,
        config.excluded_capability_count,
    )?;
    let relay_list_urls = c_str_array_to_vec_checked(
        config.relay_list_urls,
        config.relay_list_url_count,
        "relay_list_urls",
    )?;
    let bootstrap_relay_urls = c_str_array_to_vec_checked(
        config.bootstrap_relay_urls,
        config.bootstrap_relay_url_count,
        "bootstrap_relay_urls",
    )?;
    let encryption_mode = ffi_encryption_mode_to_sdk(config.encryption_mode)?;
    let gift_wrap_mode = ffi_gift_wrap_mode_to_sdk(config.gift_wrap_mode)?;

    let server_info = build_server_info(
        optional_c_str_to_string_checked(config.server_name, "server_name")?,
        optional_c_str_to_string_checked(config.server_version, "server_version")?,
        optional_c_str_to_string_checked(config.server_picture, "server_picture")?,
        optional_c_str_to_string_checked(config.server_about, "server_about")?,
        optional_c_str_to_string_checked(config.server_website, "server_website")?,
    );
    let profile_metadata = parse_profile_metadata_json(optional_c_str_to_string_checked(
        config.profile_metadata_json,
        "profile_metadata_json",
    )?)?;

    let mut sdk_config = contextvm_sdk::NostrServerTransportConfig::default()
        .with_relay_urls(if relay_urls.is_empty() {
            vec!["wss://relay.damus.io".to_string()]
        } else {
            relay_urls
        })
        .with_encryption_mode(encryption_mode)
        .with_gift_wrap_mode(gift_wrap_mode)
        .with_announced_server(config.is_announced_server)
        .with_allowed_public_keys(allowed)
        .with_session_timeout(Duration::from_secs(config.session_timeout_secs.max(1)))
        .with_cleanup_interval(Duration::from_secs(config.cleanup_interval_secs.max(1)))
        .with_publish_relay_list(config.publish_relay_list);

    if let Some(server_info) = server_info {
        sdk_config = sdk_config.with_server_info(server_info);
    }

    sdk_config = apply_extended_server_config(
        sdk_config,
        excluded,
        config.max_sessions,
        config.request_timeout_secs,
        relay_list_urls,
        bootstrap_relay_urls,
        profile_metadata,
    );

    Ok(sdk_config)
}

/// Extract fields from an FfiClientConfig and build an SDK client config.
pub fn build_sdk_client_config(
    config: &FfiClientConfig,
) -> Result<contextvm_sdk::NostrClientTransportConfig, FfiError> {
    let server_pubkey = c_str_to_string_checked(config.server_pubkey, "server_pubkey")?;
    let relay_urls =
        c_str_array_to_vec_checked(config.relay_urls, config.relay_url_count, "relay_urls")?;
    let discovery_relay_urls = c_str_array_to_vec_checked(
        config.discovery_relay_urls,
        config.discovery_relay_url_count,
        "discovery_relay_urls",
    )?;
    let fallback_operational_relay_urls = c_str_array_to_vec_checked(
        config.fallback_operational_relay_urls,
        config.fallback_operational_relay_url_count,
        "fallback_operational_relay_urls",
    )?;
    let encryption_mode = ffi_encryption_mode_to_sdk(config.encryption_mode)?;
    let gift_wrap_mode = ffi_gift_wrap_mode_to_sdk(config.gift_wrap_mode)?;

    Ok(build_sdk_client_config_from_fields(ClientConfigParts {
        relay_urls,
        server_pubkey,
        encryption_mode,
        gift_wrap_mode,
        is_stateless: config.is_stateless,
        timeout_secs: config.timeout_secs,
        discovery_relay_urls,
        fallback_operational_relay_urls,
    }))
}

/// Build a server config from UniFFI record fields.
pub fn build_sdk_server_config_from_fields(
    parts: ServerConfigParts,
) -> Result<contextvm_sdk::NostrServerTransportConfig, FfiError> {
    let server_info = build_server_info(
        parts.server_name,
        parts.server_version,
        parts.server_picture,
        parts.server_about,
        parts.server_website,
    );
    let profile_metadata = parse_profile_metadata_json(parts.profile_metadata_json)?;

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
        .with_cleanup_interval(Duration::from_secs(parts.cleanup_interval_secs.max(1)))
        .with_publish_relay_list(parts.publish_relay_list);

    if let Some(server_info) = server_info {
        sdk_config = sdk_config.with_server_info(server_info);
    }

    sdk_config = apply_extended_server_config(
        sdk_config,
        parts.excluded_capabilities,
        parts.max_sessions,
        parts.request_timeout_secs,
        parts.relay_list_urls,
        parts.bootstrap_relay_urls,
        profile_metadata,
    );

    Ok(sdk_config)
}

/// Build a client config from UniFFI record fields.
pub fn build_sdk_client_config_from_fields(
    parts: ClientConfigParts,
) -> contextvm_sdk::NostrClientTransportConfig {
    let mut config = contextvm_sdk::NostrClientTransportConfig::default()
        .with_relay_urls(parts.relay_urls)
        .with_server_pubkey(parts.server_pubkey)
        .with_encryption_mode(parts.encryption_mode)
        .with_gift_wrap_mode(parts.gift_wrap_mode)
        .with_stateless(parts.is_stateless)
        .with_timeout(Duration::from_secs(parts.timeout_secs.max(1)));

    if !parts.discovery_relay_urls.is_empty() {
        config = config.with_discovery_relay_urls(parts.discovery_relay_urls);
    }
    if !parts.fallback_operational_relay_urls.is_empty() {
        config = config.with_fallback_operational_relay_urls(parts.fallback_operational_relay_urls);
    }

    config
}

fn apply_extended_server_config(
    mut config: contextvm_sdk::NostrServerTransportConfig,
    excluded_capabilities: Vec<CapabilityExclusionParts>,
    max_sessions: usize,
    request_timeout_secs: u64,
    relay_list_urls: Vec<String>,
    bootstrap_relay_urls: Vec<String>,
    profile_metadata: Option<contextvm_sdk::ProfileMetadata>,
) -> contextvm_sdk::NostrServerTransportConfig {
    if !excluded_capabilities.is_empty() {
        config = config.with_excluded_capabilities(
            excluded_capabilities
                .into_iter()
                .map(|cap| contextvm_sdk::CapabilityExclusion {
                    method: cap.method,
                    name: cap.name,
                })
                .collect(),
        );
    }
    if max_sessions > 0 {
        config = config.with_max_sessions(max_sessions);
    }
    if request_timeout_secs > 0 {
        config = config.with_request_timeout(Duration::from_secs(request_timeout_secs));
    }
    if !relay_list_urls.is_empty() {
        config = config.with_relay_list_urls(relay_list_urls);
    }
    if !bootstrap_relay_urls.is_empty() {
        config = config.with_bootstrap_relay_urls(bootstrap_relay_urls);
    }
    if let Some(profile_metadata) = profile_metadata {
        config = config.with_profile_metadata(profile_metadata);
    }
    config
}

fn ffi_capability_exclusions_to_parts(
    ptr: *mut FfiCapabilityExclusion,
    count: usize,
) -> Result<Vec<CapabilityExclusionParts>, FfiError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err(FfiError {
            code: ErrorCode::Validation,
            message: format!("excluded_capabilities has count {count} but null pointer"),
        });
    }

    unsafe {
        std::slice::from_raw_parts(ptr, count)
            .iter()
            .map(|cap| {
                let method = c_str_to_string_checked(cap.method, "capability_exclusions[].method")?;
                Ok(CapabilityExclusionParts {
                    method,
                    name: optional_c_str_to_string_checked(
                        cap.name,
                        "capability_exclusions[].name",
                    )?,
                })
            })
            .collect()
    }
}

fn parse_profile_metadata_json(
    json: Option<String>,
) -> Result<Option<contextvm_sdk::ProfileMetadata>, FfiError> {
    match json {
        Some(json) if !json.trim().is_empty() => {
            serde_json::from_str(&json).map(Some).map_err(|e| FfiError {
                code: ErrorCode::Serialization,
                message: format!("invalid profile_metadata_json: {e}"),
            })
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ENCRYPTION_MODE_OPTIONAL, GIFT_WRAP_MODE_OPTIONAL};
    use std::ffi::CString;
    use std::os::raw::c_char;
    use std::ptr;

    fn minimal_ffi_server_config() -> FfiServerConfig {
        FfiServerConfig {
            relay_urls: ptr::null_mut(),
            relay_url_count: 0,
            encryption_mode: ENCRYPTION_MODE_OPTIONAL,
            gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
            is_announced_server: false,
            server_name: ptr::null_mut(),
            server_version: ptr::null_mut(),
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
            request_timeout_secs: 0,
            relay_list_urls: ptr::null_mut(),
            relay_list_url_count: 0,
            bootstrap_relay_urls: ptr::null_mut(),
            bootstrap_relay_url_count: 0,
            publish_relay_list: false,
            profile_metadata_json: ptr::null_mut(),
        }
    }

    fn minimal_ffi_client_config(server_pubkey: *mut c_char) -> FfiClientConfig {
        FfiClientConfig {
            relay_urls: ptr::null_mut(),
            relay_url_count: 0,
            server_pubkey,
            encryption_mode: ENCRYPTION_MODE_OPTIONAL,
            gift_wrap_mode: GIFT_WRAP_MODE_OPTIONAL,
            is_stateless: false,
            timeout_secs: 30,
            discovery_relay_urls: ptr::null_mut(),
            discovery_relay_url_count: 0,
            fallback_operational_relay_urls: ptr::null_mut(),
            fallback_operational_relay_url_count: 0,
        }
    }

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
            excluded_capabilities: vec![],
            max_sessions: 0,
            request_timeout_secs: 0,
            relay_list_urls: vec![],
            bootstrap_relay_urls: vec![],
            publish_relay_list: true,
            profile_metadata_json: None,
        })
        .expect("config should build");

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

    #[test]
    fn server_config_preserves_extended_fields() {
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
            excluded_capabilities: vec![CapabilityExclusionParts {
                method: "tools/call".to_string(),
                name: Some("get_weather".to_string()),
            }],
            max_sessions: 42,
            request_timeout_secs: 17,
            relay_list_urls: vec!["wss://relay-list.example.com".to_string()],
            bootstrap_relay_urls: vec!["wss://bootstrap.example.com".to_string()],
            publish_relay_list: false,
            profile_metadata_json: Some(r#"{"name":"profile","nip05":"bot@example.com"}"#.into()),
        })
        .expect("config should build");

        assert_eq!(config.excluded_capabilities.len(), 1);
        assert_eq!(config.max_sessions, 42);
        assert_eq!(config.request_timeout.as_secs(), 17);
        assert_eq!(
            config.relay_list_urls.as_deref(),
            Some(&["wss://relay-list.example.com".to_string()][..])
        );
        assert_eq!(
            config.bootstrap_relay_urls.as_deref(),
            Some(&["wss://bootstrap.example.com".to_string()][..])
        );
        assert!(!config.publish_relay_list);
        let profile = config.profile_metadata.expect("profile metadata");
        assert_eq!(profile.name.as_deref(), Some("profile"));
        assert_eq!(profile.nip05.as_deref(), Some("bot@example.com"));
    }

    #[test]
    fn client_config_preserves_discovery_relays() {
        let config = build_sdk_client_config_from_fields(ClientConfigParts {
            relay_urls: vec!["wss://relay.example.com".to_string()],
            server_pubkey: "abc".to_string(),
            encryption_mode: contextvm_sdk::EncryptionMode::Optional,
            gift_wrap_mode: contextvm_sdk::GiftWrapMode::Optional,
            is_stateless: false,
            timeout_secs: 30,
            discovery_relay_urls: vec!["wss://discovery.example.com".to_string()],
            fallback_operational_relay_urls: vec!["wss://fallback.example.com".to_string()],
        });

        assert_eq!(
            config.discovery_relay_urls.as_deref(),
            Some(&["wss://discovery.example.com".to_string()][..])
        );
        assert_eq!(
            config.fallback_operational_relay_urls.as_deref(),
            Some(&["wss://fallback.example.com".to_string()][..])
        );
    }

    #[test]
    fn ffi_server_config_rejects_counted_null_allowlist_pointer() {
        let mut config = minimal_ffi_server_config();
        config.allowed_pubkey_count = 1;

        let err = build_sdk_server_config(&config).expect_err("counted null allowlist must fail");

        assert_eq!(err.code, ErrorCode::Validation);
        assert!(err.message.contains("allowed_pubkeys"));
    }

    #[test]
    fn ffi_server_config_rejects_null_allowlist_entry() {
        let mut config = minimal_ffi_server_config();
        let mut entries = [ptr::null_mut()];
        config.allowed_pubkeys = entries.as_mut_ptr();
        config.allowed_pubkey_count = entries.len();

        let err = build_sdk_server_config(&config).expect_err("null allowlist entry must fail");

        assert_eq!(err.code, ErrorCode::Validation);
        assert!(err.message.contains("allowed_pubkeys[0]"));
    }

    #[test]
    fn ffi_server_config_rejects_non_utf8_allowlist_entry() {
        let mut config = minimal_ffi_server_config();
        let bad = CString::new(vec![0xff]).expect("no interior nul");
        let mut entries = [bad.as_ptr() as *mut c_char];
        config.allowed_pubkeys = entries.as_mut_ptr();
        config.allowed_pubkey_count = entries.len();

        let err =
            build_sdk_server_config(&config).expect_err("non-UTF-8 allowlist entry must fail");

        assert_eq!(err.code, ErrorCode::Validation);
        assert!(err.message.contains("allowed_pubkeys[0]"));
    }

    #[test]
    fn ffi_server_config_rejects_invalid_modes() {
        let mut config = minimal_ffi_server_config();
        config.encryption_mode = 99;

        let err = build_sdk_server_config(&config).expect_err("invalid encryption mode must fail");

        assert_eq!(err.code, ErrorCode::Validation);
        assert!(err.message.contains("encryption_mode"));

        let mut config = minimal_ffi_server_config();
        config.gift_wrap_mode = 99;

        let err = build_sdk_server_config(&config).expect_err("invalid gift-wrap mode must fail");

        assert_eq!(err.code, ErrorCode::Validation);
        assert!(err.message.contains("gift_wrap_mode"));
    }

    #[test]
    fn ffi_client_config_rejects_missing_pubkey_and_invalid_modes() {
        let config = minimal_ffi_client_config(ptr::null_mut());

        let err = build_sdk_client_config(&config).expect_err("missing server pubkey must fail");

        assert_eq!(err.code, ErrorCode::Validation);
        assert!(err.message.contains("server_pubkey"));

        let server_pubkey = CString::new("abc").expect("valid c string");
        let mut config = minimal_ffi_client_config(server_pubkey.as_ptr() as *mut c_char);
        config.encryption_mode = 99;

        let err =
            build_sdk_client_config(&config).expect_err("invalid client encryption mode must fail");

        assert_eq!(err.code, ErrorCode::Validation);
        assert!(err.message.contains("encryption_mode"));
    }
}
