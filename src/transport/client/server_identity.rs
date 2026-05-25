//! Server identity parsing for CEP-17 client-side relay discovery.
//!
//! Accepts hex pubkeys, npub (NIP-19), or nprofile (NIP-19) strings and
//! extracts the server's public key and any embedded relay hints.
//! Mirrors the TS SDK's `parseServerIdentity()`.

use nostr_sdk::prelude::*;

use crate::core::error::{Error, Result};

/// Parse a server identity string into a public key and optional relay hints.
///
/// Supported formats:
/// - **Hex**: 64-character hex-encoded public key
/// - **npub**: NIP-19 bech32-encoded public key
/// - **nprofile**: NIP-19 bech32-encoded profile (pubkey + relay hints)
///
/// Returns `(PublicKey, Vec<String>)` where the second element contains relay
/// hint URLs extracted from an nprofile, or an empty vec for hex/npub.
pub fn parse_server_identity(input: &str) -> Result<(PublicKey, Vec<String>)> {
    // Try hex first
    if let Ok(pk) = PublicKey::from_hex(input) {
        return Ok((pk, vec![]));
    }

    // Try bech32 (npub / nprofile)
    match Nip19::from_bech32(input) {
        Ok(Nip19::Pubkey(pk)) => Ok((pk, vec![])),
        Ok(Nip19::Profile(profile)) => {
            let relays: Vec<String> = profile.relays.into_iter().map(|r| r.to_string()).collect();
            Ok((profile.public_key, relays))
        }
        _ => Err(Error::Other(format!(
            "Invalid serverPubkey format: {input}. Expected hex pubkey, npub, or nprofile."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_pubkey_roundtrip() {
        let keys = Keys::generate();
        let hex = keys.public_key().to_hex();
        let (pk, hints) = parse_server_identity(&hex).unwrap();
        assert_eq!(pk, keys.public_key());
        assert!(hints.is_empty());
    }

    #[test]
    fn npub_roundtrip() {
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32().unwrap();
        let (pk, hints) = parse_server_identity(&npub).unwrap();
        assert_eq!(pk, keys.public_key());
        assert!(hints.is_empty());
    }

    #[test]
    fn nprofile_with_relays() {
        let keys = Keys::generate();
        let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
        let relay2 = RelayUrl::parse("wss://relay2.example.com").unwrap();
        let profile = Nip19Profile::new(keys.public_key(), vec![relay1.clone(), relay2.clone()]);
        let nprofile = profile.to_bech32().unwrap();

        let (pk, hints) = parse_server_identity(&nprofile).unwrap();
        assert_eq!(pk, keys.public_key());
        assert_eq!(hints.len(), 2);
        assert!(hints.contains(&relay1.to_string()));
        assert!(hints.contains(&relay2.to_string()));
    }

    #[test]
    fn nprofile_without_relays() {
        let keys = Keys::generate();
        let profile = Nip19Profile::new(keys.public_key(), Vec::<RelayUrl>::new());
        let nprofile = profile.to_bech32().unwrap();

        let (pk, hints) = parse_server_identity(&nprofile).unwrap();
        assert_eq!(pk, keys.public_key());
        assert!(hints.is_empty());
    }

    #[test]
    fn invalid_string_returns_error() {
        let result = parse_server_identity("not-a-valid-key");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid serverPubkey format"));
        assert!(err.contains("Expected hex pubkey, npub, or nprofile"));
    }

    #[test]
    fn invalid_npub_checksum_returns_error() {
        // Valid npub prefix but corrupted data
        let result = parse_server_identity("npub1invalidchecksum");
        assert!(result.is_err());
    }
}
