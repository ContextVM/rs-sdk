//! Nostr signer utilities.
//!
//! Re-exports key types from nostr-sdk and provides convenience constructors.

pub use nostr_sdk::prelude::{Keys, NostrSigner, PublicKey};

/// Create keys from a private key string (hex or nsec/bech32).
pub fn from_sk(sk: &str) -> std::result::Result<Keys, nostr_sdk::key::Error> {
    Keys::parse(sk)
}

/// Generate a new random keypair.
pub fn generate() -> Keys {
    Keys::generate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_produces_valid_keys() {
        let keys = generate();
        let pubkey = keys.public_key();
        // Public key hex should be 64 chars
        assert_eq!(pubkey.to_hex().len(), 64);
    }

    #[test]
    fn test_generate_produces_unique_keys() {
        let k1 = generate();
        let k2 = generate();
        assert_ne!(k1.public_key(), k2.public_key());
    }

    #[test]
    fn test_from_sk_hex() {
        let keys = generate();
        let sk_hex = keys.secret_key().to_secret_hex();
        let restored = from_sk(&sk_hex).unwrap();
        assert_eq!(restored.public_key(), keys.public_key());
    }

    #[test]
    fn test_from_sk_nsec() {
        let keys = generate();
        use nostr_sdk::ToBech32;
        let nsec = keys.secret_key().to_bech32().unwrap();
        let restored = from_sk(&nsec).unwrap();
        assert_eq!(restored.public_key(), keys.public_key());
    }

    #[test]
    fn test_from_sk_invalid() {
        assert!(from_sk("not_a_valid_key").is_err());
        assert!(from_sk("").is_err());
    }
}
