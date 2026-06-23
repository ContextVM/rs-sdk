//! Shared discovery helpers for C and UniFFI bindings.

use contextvm_sdk::signer::PublicKey;
use nostr_sdk::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct DiscoveredToolRecord {
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

#[derive(Debug, Clone)]
pub(crate) struct ProviderProfileRecord {
    pub pubkey: String,
    pub name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub nip05: Option<String>,
}

pub(crate) async fn discover_tools(
    client: &Arc<Client>,
    provider_pubkey: &str,
    provider_display_name: Option<String>,
    relay_urls: &[String],
) -> contextvm_sdk::Result<Vec<DiscoveredToolRecord>> {
    let parsed_pubkey = PublicKey::from_hex(provider_pubkey)
        .map_err(|e| contextvm_sdk::Error::Validation(format!("bad pubkey: {e}")))?;
    let raw_tools =
        contextvm_sdk::discovery::discover_tools(client, &parsed_pubkey, relay_urls).await?;

    raw_tools
        .into_iter()
        .map(|tool| tool_record_from_value(provider_pubkey, provider_display_name.clone(), tool))
        .collect()
}

pub(crate) async fn discover_all_tools(
    client: &Arc<Client>,
    relay_urls: &[String],
) -> contextvm_sdk::Result<Vec<DiscoveredToolRecord>> {
    let servers = contextvm_sdk::discovery::discover_servers(client, relay_urls).await?;
    let mut tools = Vec::new();

    for server in servers {
        match discover_tools(
            client,
            &server.pubkey,
            server.server_info.name.clone(),
            relay_urls,
        )
        .await
        {
            Ok(mut server_tools) => tools.append(&mut server_tools),
            Err(_) => continue,
        }
    }

    let provider_pubkeys: Vec<String> = tools
        .iter()
        .map(|tool| tool.provider_pubkey.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if let Ok(profiles) = fetch_provider_profiles(client, &provider_pubkeys, relay_urls).await {
        for tool in &mut tools {
            if let Some(profile) = profiles.get(&tool.provider_pubkey) {
                tool.provider_name = profile.name.clone();
                tool.provider_about = profile.about.clone();
                tool.provider_picture = profile.picture.clone();
                tool.provider_nip05 = profile.nip05.clone();
            }
        }
    }

    Ok(tools)
}

pub(crate) async fn fetch_provider_profiles(
    client: &Arc<Client>,
    provider_pubkeys: &[String],
    relay_urls: &[String],
) -> contextvm_sdk::Result<HashMap<String, ProviderProfileRecord>> {
    let parsed_pubkeys: Vec<PublicKey> = provider_pubkeys
        .iter()
        .filter_map(|pubkey| PublicKey::from_hex(pubkey).ok())
        .collect();

    if parsed_pubkeys.is_empty() {
        return Ok(HashMap::new());
    }

    let filter = Filter::new().authors(parsed_pubkeys).kind(Kind::Metadata);
    let timeout = Duration::from_secs(10);
    let events = if relay_urls.is_empty() {
        client.fetch_events(filter, timeout).await
    } else {
        client.fetch_events_from(relay_urls, filter, timeout).await
    }
    .map_err(|e| contextvm_sdk::Error::Transport(e.to_string()))?;

    let mut profiles = HashMap::new();
    for event in events {
        if let Some(profile) = profile_from_metadata(event.pubkey.to_hex(), &event.content) {
            profiles.insert(profile.pubkey.clone(), profile);
        }
    }

    Ok(profiles)
}

pub(crate) fn pubkey_hex_to_npub(pubkey_hex: &str) -> contextvm_sdk::Result<String> {
    use nostr_sdk::ToBech32;

    let pubkey = PublicKey::from_hex(pubkey_hex)
        .map_err(|e| contextvm_sdk::Error::Validation(format!("bad pubkey: {e}")))?;
    pubkey
        .to_bech32()
        .map_err(|e| contextvm_sdk::Error::Other(e.to_string()))
}

fn tool_record_from_value(
    provider_pubkey: &str,
    provider_display_name: Option<String>,
    value: serde_json::Value,
) -> contextvm_sdk::Result<DiscoveredToolRecord> {
    let tool_name = value
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| contextvm_sdk::Error::Other("tool announcement missing name".into()))?
        .to_string();
    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let schema = value
        .get("inputSchema")
        .or_else(|| value.get("input_schema"))
        .ok_or_else(|| {
            contextvm_sdk::Error::Other(format!(
                "tool announcement {tool_name} missing inputSchema"
            ))
        })?;
    let schema_json = serde_json::to_string(schema).map_err(contextvm_sdk::Error::Serialization)?;

    Ok(DiscoveredToolRecord {
        provider_pubkey: provider_pubkey.to_string(),
        provider_display_name,
        provider_name: None,
        provider_about: None,
        provider_picture: None,
        provider_nip05: None,
        tool_name,
        description,
        schema_json,
    })
}

fn profile_from_metadata(pubkey: String, content: &str) -> Option<ProviderProfileRecord> {
    let metadata: serde_json::Value = serde_json::from_str(content).ok()?;
    Some(ProviderProfileRecord {
        pubkey,
        name: metadata
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from),
        about: metadata
            .get("about")
            .and_then(|v| v.as_str())
            .map(String::from),
        picture: metadata
            .get("picture")
            .and_then(|v| v.as_str())
            .map(String::from),
        nip05: metadata
            .get("nip05")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_record_preserves_confidential_app_fields() {
        let tool = tool_record_from_value(
            "abc",
            Some("provider".to_string()),
            json!({
                "name": "echo",
                "description": "Echo a message",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" }
                    }
                }
            }),
        )
        .unwrap();

        assert_eq!(tool.provider_pubkey, "abc");
        assert_eq!(tool.provider_display_name.as_deref(), Some("provider"));
        assert_eq!(tool.tool_name, "echo");
        assert_eq!(tool.description, "Echo a message");
        assert!(tool.schema_json.contains("message"));
    }

    #[test]
    fn profile_metadata_maps_provider_fields() {
        let profile = profile_from_metadata(
            "abc".to_string(),
            r#"{"name":"Provider","about":"About","picture":"https://pic","nip05":"p@example.com"}"#,
        )
        .unwrap();

        assert_eq!(profile.name.as_deref(), Some("Provider"));
        assert_eq!(profile.about.as_deref(), Some("About"));
        assert_eq!(profile.picture.as_deref(), Some("https://pic"));
        assert_eq!(profile.nip05.as_deref(), Some("p@example.com"));
    }

    #[test]
    fn pubkey_hex_to_npub_rejects_invalid_hex() {
        assert!(pubkey_hex_to_npub("not-hex").is_err());
    }
}
