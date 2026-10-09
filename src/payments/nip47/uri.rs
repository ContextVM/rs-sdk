//! NWC connection-URI parsing (NIP-47).
//!
//! [`NostrWalletConnectURI::parse`](nostr_sdk::nips::nip47::NostrWalletConnectURI::parse)
//! reads the wallet pubkey from the URI *host* only, so it rejects the
//! `nostr+walletconnect:<pubkey>?...` form (no `//`, pubkey in the path) that
//! ts-sdk accepts, and it rejects the legacy `nostrwalletconnect://` scheme
//! outright. Wallets in the wild emit all three, and a connection string the
//! ts-sdk takes must not fail here, so this module does its own parsing and
//! builds a [`NwcConnection`] directly. Nothing is handed to the upstream
//! type.
//!
//! Parity note: ts `parseNwcConnectionString` prefers the pathname and falls
//! back to the host (`url.pathname?.replace(/^\//, '') || url.host`). This
//! reproduces that precedence exactly.

use nostr_sdk::prelude::*;

use crate::payments::nip47::error::NwcError;

/// Canonical URI scheme for a NWC connection string.
const NWC_URI_SCHEME: &str = "nostr+walletconnect";
/// Legacy scheme still emitted by some wallets and accepted by ts-sdk's
/// `new URL()` parse, which never checks the protocol.
const NWC_URI_SCHEME_LEGACY: &str = "nostrwalletconnect";

/// A parsed NWC connection string: which wallet to talk to, over which relays,
/// under which client identity.
#[derive(Debug, Clone)]
pub struct NwcConnection {
    /// Wallet service public key.
    pub wallet_pubkey: PublicKey,
    /// Relay URLs the wallet service is reachable on. Never empty.
    pub relays: Vec<RelayUrl>,
    /// Client secret key, used to sign requests and to encrypt to the wallet.
    pub secret: SecretKey,
    /// Optional lightning address advertised by the wallet (LUD-16).
    pub lud16: Option<String>,
}

impl NwcConnection {
    /// The client public key derived from [`Self::secret`]; the `p`-tag a
    /// wallet addresses its responses and notifications to.
    pub fn client_pubkey(&self) -> PublicKey {
        Keys::new(self.secret.clone()).public_key()
    }

    /// Relay URLs as strings, for [`crate::relay::RelayPoolTrait::connect`].
    pub fn relay_urls(&self) -> Vec<String> {
        self.relays.iter().map(|r| r.to_string()).collect()
    }
}

fn invalid(reason: &str) -> NwcError {
    NwcError::Config {
        reason: format!("invalid NWC connection string: {reason}"),
    }
}

/// Parse a NWC connection string.
///
/// Accepts both shapes seen in the wild:
///
/// ```text
/// nostr+walletconnect://<wallet_pubkey>?relay=wss://relay.example&secret=<hex>
/// nostr+walletconnect:<wallet_pubkey>?relay=wss://relay.example&secret=<hex>
/// ```
///
/// `relay` may repeat. `secret` is a 32-byte hex secret key. `lud16` is
/// optional and passed through.
///
/// # Errors
///
/// Returns [`NwcError::Config`] when the scheme is neither
/// `nostr+walletconnect` nor the legacy `nostrwalletconnect`, the pubkey or
/// secret is missing or malformed, or no relay is present. A relay value that does not parse is skipped, matching
/// upstream; an empty resulting relay set is an error.
pub fn parse_nwc_uri(uri: &str) -> Result<NwcConnection, NwcError> {
    let url = Url::parse(uri.trim()).map_err(|_| invalid("not a URL"))?;

    if url.scheme() != NWC_URI_SCHEME && url.scheme() != NWC_URI_SCHEME_LEGACY {
        return Err(invalid(
            "scheme is neither nostr+walletconnect nor nostrwalletconnect",
        ));
    }

    // ts precedence: a non-empty pathname wins, else the host. The `//` form
    // puts the pubkey in the host and leaves the path empty; the schema-only
    // form puts it in the path and leaves the host unset.
    let from_path = url.path().trim_start_matches('/').trim().to_string();
    let raw_pubkey = if from_path.is_empty() {
        url.host_str().unwrap_or_default().trim().to_string()
    } else {
        from_path
    };

    if raw_pubkey.is_empty() {
        return Err(invalid("missing wallet pubkey"));
    }

    let wallet_pubkey =
        PublicKey::from_hex(&raw_pubkey).map_err(|_| invalid("wallet pubkey is not valid hex"))?;

    let mut relays: Vec<RelayUrl> = Vec::new();
    let mut secret: Option<SecretKey> = None;
    let mut lud16: Option<String> = None;

    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "relay" => {
                // Skip unparseable relay values rather than failing the whole
                // URI, matching upstream `NostrWalletConnectURI::parse`.
                if let Ok(relay) = RelayUrl::parse(value.trim()) {
                    relays.push(relay);
                }
            }
            // Last-wins on a duplicate, deliberately: a later parameter is
            // the more likely correction, and ts's `searchParams.get` takes
            // the FIRST. Divergence noted rather than inherited, since no
            // conformant wallet emits two.
            "secret" => secret = SecretKey::from_hex(value.trim()).ok(),
            "lud16" => lud16 = Some(value.to_string()),
            _ => {}
        }
    }

    let secret = secret.ok_or_else(|| invalid("missing or malformed secret"))?;

    if relays.is_empty() {
        return Err(invalid("no usable relay"));
    }

    Ok(NwcConnection {
        wallet_pubkey,
        relays,
        secret,
        lud16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBKEY: &str = "b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4";
    const SECRET: &str = "71a8c14c1407c113601079c4302dab36460f0ccd0ad506f1f2dc73b5100e4f3c";

    fn host_form() -> String {
        format!("nostr+walletconnect://{PUBKEY}?relay=wss%3A%2F%2Frelay.example&secret={SECRET}")
    }

    fn path_form() -> String {
        format!("nostr+walletconnect:{PUBKEY}?relay=wss%3A%2F%2Frelay.example&secret={SECRET}")
    }

    #[test]
    fn parses_host_form() {
        let c = parse_nwc_uri(&host_form()).expect("host form should parse");
        assert_eq!(c.wallet_pubkey.to_hex(), PUBKEY);
        assert_eq!(c.secret.to_secret_hex(), SECRET);
        assert_eq!(c.relays.len(), 1);
    }

    /// The whole reason this module exists: upstream rejects this form, ts accepts it.
    #[test]
    fn parses_path_form_that_upstream_rejects() {
        assert!(
            nostr_sdk::nips::nip47::NostrWalletConnectURI::parse(path_form()).is_err(),
            "precondition: upstream is expected to reject the pathname form"
        );

        let c = parse_nwc_uri(&path_form()).expect("path form should parse");
        assert_eq!(c.wallet_pubkey.to_hex(), PUBKEY);
        assert_eq!(c.secret.to_secret_hex(), SECRET);
    }

    #[test]
    fn both_forms_agree() {
        let a = parse_nwc_uri(&host_form()).unwrap();
        let b = parse_nwc_uri(&path_form()).unwrap();
        assert_eq!(a.wallet_pubkey, b.wallet_pubkey);
        assert_eq!(a.secret.to_secret_hex(), b.secret.to_secret_hex());
        assert_eq!(a.relay_urls(), b.relay_urls());
    }

    #[test]
    fn collects_repeated_relays_in_order() {
        let uri = format!(
            "nostr+walletconnect://{PUBKEY}?relay=wss%3A%2F%2Fa.example&relay=wss%3A%2F%2Fb.example&secret={SECRET}"
        );
        let c = parse_nwc_uri(&uri).unwrap();
        let urls = c.relay_urls();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("a.example"));
        assert!(urls[1].contains("b.example"));
    }

    #[test]
    fn passes_through_lud16() {
        let uri = format!(
            "nostr+walletconnect://{PUBKEY}?relay=wss%3A%2F%2Fa.example&secret={SECRET}&lud16=me%40example.com"
        );
        assert_eq!(
            parse_nwc_uri(&uri).unwrap().lud16.as_deref(),
            Some("me@example.com")
        );
    }

    #[test]
    fn client_pubkey_is_derived_from_secret() {
        let c = parse_nwc_uri(&host_form()).unwrap();
        let expected = Keys::new(SecretKey::from_hex(SECRET).unwrap()).public_key();
        assert_eq!(c.client_pubkey(), expected);
        assert_ne!(
            c.client_pubkey(),
            c.wallet_pubkey,
            "client identity is not the wallet identity"
        );
    }

    /// Some wallets still emit the pre-standard scheme, and ts accepts it
    /// because `new URL()` never checks the protocol.
    #[test]
    fn parses_the_legacy_nostrwalletconnect_scheme() {
        let uri = format!(
            "nostrwalletconnect://{PUBKEY}?relay=wss%3A%2F%2Frelay.example&secret={SECRET}"
        );
        assert!(
            nostr_sdk::nips::nip47::NostrWalletConnectURI::parse(&uri).is_err(),
            "precondition: upstream rejects the legacy scheme"
        );
        let c = parse_nwc_uri(&uri).expect("legacy scheme should parse");
        assert_eq!(c.wallet_pubkey.to_hex(), PUBKEY);
        assert_eq!(c.secret.to_secret_hex(), SECRET);
    }

    /// ts-sdk's own tested shape, verbatim from `connection.test.ts`.
    #[test]
    fn parses_the_shape_ts_pins_in_its_tests() {
        let uri =
            format!("nostr+walletconnect://{PUBKEY}?relay=wss://relay.example&secret={SECRET}");
        let c = parse_nwc_uri(&uri).expect("ts's shape must parse");
        assert_eq!(c.wallet_pubkey.to_hex(), PUBKEY);
        assert_eq!(c.relays.len(), 1);
    }

    /// Deliberate divergence from ts, which takes the first.
    #[test]
    fn a_duplicate_secret_is_last_wins() {
        let other = "0000000000000000000000000000000000000000000000000000000000000002";
        let uri = format!(
            "nostr+walletconnect://{PUBKEY}?relay=wss%3A%2F%2Fa.example&secret={other}&secret={SECRET}"
        );
        let c = parse_nwc_uri(&uri).unwrap();
        assert_eq!(c.secret.to_secret_hex(), SECRET, "the later secret wins");
    }

    #[test]
    fn rejects_wrong_scheme() {
        let uri = format!("https://{PUBKEY}?relay=wss%3A%2F%2Fa.example&secret={SECRET}");
        assert!(parse_nwc_uri(&uri).is_err());
    }

    #[test]
    fn rejects_missing_secret() {
        let uri = format!("nostr+walletconnect://{PUBKEY}?relay=wss%3A%2F%2Fa.example");
        assert!(parse_nwc_uri(&uri).is_err());
    }

    #[test]
    fn rejects_missing_relay() {
        let uri = format!("nostr+walletconnect://{PUBKEY}?secret={SECRET}");
        assert!(parse_nwc_uri(&uri).is_err());
    }

    #[test]
    fn rejects_malformed_pubkey() {
        let uri =
            format!("nostr+walletconnect://not-hex?relay=wss%3A%2F%2Fa.example&secret={SECRET}");
        assert!(parse_nwc_uri(&uri).is_err());
    }

    #[test]
    fn rejects_malformed_secret() {
        let uri = format!("nostr+walletconnect://{PUBKEY}?relay=wss%3A%2F%2Fa.example&secret=nope");
        assert!(parse_nwc_uri(&uri).is_err());
    }

    #[test]
    fn rejects_when_every_relay_is_unparseable() {
        let uri = format!("nostr+walletconnect://{PUBKEY}?relay=%20&secret={SECRET}");
        assert!(parse_nwc_uri(&uri).is_err());
    }
}
