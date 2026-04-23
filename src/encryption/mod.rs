//! Encryption and gift wrapping for ContextVM.
//!
//! Provides NIP-44 encryption/decryption and NIP-59 gift wrapping.
//! The actual gift wrapping is done via nostr-sdk's Client for full NIP-59 compliance.

use crate::core::constants::{EPHEMERAL_GIFT_WRAP_KIND, GIFT_WRAP_KIND};
use crate::core::error::{Error, Result};
use nostr_sdk::prelude::*;

/// Encrypt a message using NIP-44.
pub async fn encrypt_nip44<T>(
    signer: &T,
    receiver_pubkey: &PublicKey,
    plaintext: &str,
) -> Result<String>
where
    T: NostrSigner,
{
    signer
        .nip44_encrypt(receiver_pubkey, plaintext)
        .await
        .map_err(|e| Error::Encryption(e.to_string()))
}

/// Decrypt a message using NIP-44.
pub async fn decrypt_nip44<T>(
    signer: &T,
    sender_pubkey: &PublicKey,
    ciphertext: &str,
) -> Result<String>
where
    T: NostrSigner,
{
    signer
        .nip44_decrypt(sender_pubkey, ciphertext)
        .await
        .map_err(|e| Error::Decryption(e.to_string()))
}

// Decrypt a single-layer NIP-44 gift wrap (kind 1059).

pub async fn decrypt_gift_wrap_single_layer<T>(signer: &T, event: &Event) -> Result<String>
where
    T: NostrSigner,
{
    let sender_pubkey = event.pubkey;
    decrypt_nip44(signer, &sender_pubkey, &event.content).await
}

// Create a single-layer NIP-44 gift wrap (kind 1059).

pub async fn gift_wrap_single_layer<T>(
    _signer: &T,
    recipient: &PublicKey,
    plaintext: &str,
) -> Result<Event>
where
    T: NostrSigner,
{
    let ephemeral = Keys::generate();

    let encrypted = encrypt_nip44(&ephemeral, recipient, plaintext).await?;

    let builder =
        EventBuilder::new(Kind::Custom(GIFT_WRAP_KIND), encrypted).tag(Tag::public_key(*recipient));

    builder
        .sign_with_keys(&ephemeral)
        .map_err(|e| Error::Encryption(e.to_string()))
}

/// Create a single-layer NIP-44 gift wrap using the provided outer event kind.
///
/// Only ContextVM's supported persistent (`1059`) and ephemeral (`21059`) gift-wrap
/// kinds are accepted here.
pub async fn gift_wrap_single_layer_with_kind<T>(
    _signer: &T,
    recipient: &PublicKey,
    plaintext: &str,
    gift_wrap_kind: u16,
) -> Result<Event>
where
    T: NostrSigner,
{
    if gift_wrap_kind != GIFT_WRAP_KIND && gift_wrap_kind != EPHEMERAL_GIFT_WRAP_KIND {
        return Err(Error::Encryption(format!(
            "Unsupported gift-wrap kind for single-layer encryption: {gift_wrap_kind}"
        )));
    }

    let ephemeral = Keys::generate();

    let encrypted = encrypt_nip44(&ephemeral, recipient, plaintext).await?;

    let builder =
        EventBuilder::new(Kind::Custom(gift_wrap_kind), encrypted).tag(Tag::public_key(*recipient));

    builder
        .sign_with_keys(&ephemeral)
        .map_err(|e| Error::Encryption(e.to_string()))
}

// Legacy NIP-59 functions kept for reference but deprecated.

/// Decrypt a full NIP-59 gift-wrapped event using the Client.
///
/// **Deprecated**: Use `decrypt_gift_wrap_single_layer` for ContextVM interop.
/// This expects the full NIP-59 two-layer scheme (gift wrap → seal → rumor).
#[deprecated(note = "Use decrypt_gift_wrap_single_layer for ContextVM compatibility")]
pub async fn decrypt_gift_wrap(client: &Client, event: &Event) -> Result<UnsignedEvent> {
    let unwrapped = client
        .unwrap_gift_wrap(event)
        .await
        .map_err(|e| Error::Decryption(e.to_string()))?;
    Ok(unwrapped.rumor)
}

/// Create and publish a full NIP-59 gift-wrapped event.
///
/// **Deprecated**: Use `gift_wrap_single_layer` for ContextVM compatibility.
#[deprecated(note = "Use gift_wrap_single_layer for ContextVM compatibility")]
pub async fn gift_wrap(
    client: &Client,
    recipient: &PublicKey,
    rumor: UnsignedEvent,
) -> Result<EventId> {
    let output = client
        .gift_wrap(recipient, rumor, Vec::<Tag>::new())
        .await
        .map_err(|e| Error::Encryption(e.to_string()))?;
    Ok(output.val)
}

#[cfg(test)]
mod tests {
    use crate::core::constants::{EPHEMERAL_GIFT_WRAP_KIND, GIFT_WRAP_KIND};

    use super::*;

    #[tokio::test]
    async fn test_nip44_roundtrip() {
        let keys1 = Keys::generate();
        let keys2 = Keys::generate();

        let plaintext = "Hello, ContextVM!";

        let ciphertext = encrypt_nip44(&keys1, &keys2.public_key(), plaintext)
            .await
            .unwrap();

        let decrypted = decrypt_nip44(&keys2, &keys1.public_key(), &ciphertext)
            .await
            .unwrap();

        assert_eq!(plaintext, decrypted);
    }

    /// Create a gift wrap event the same way the JS/TS SDK does:
    /// single-layer NIP-44 encryption with an ephemeral key.
    ///
    /// JS SDK `encryptMessage`:
    ///   1. Generate ephemeral keypair
    ///   2. NIP-44 encrypt the plaintext using ephemeral_secret + recipient_pubkey
    ///   3. Build kind 1059 event with encrypted content, `p` tag = recipient
    ///   4. Sign with ephemeral key
    async fn create_simple_gift_wrap(plaintext: &str, recipient: &PublicKey) -> (Event, Keys) {
        let ephemeral = Keys::generate();

        // Single-layer NIP-44 encrypt
        let encrypted = encrypt_nip44(&ephemeral, recipient, plaintext)
            .await
            .unwrap();

        // Build kind 1059 event
        let builder = EventBuilder::new(Kind::from(GIFT_WRAP_KIND), encrypted)
            .tag(Tag::public_key(*recipient));

        let event = builder.sign_with_keys(&ephemeral).unwrap();
        (event, ephemeral)
    }

    #[tokio::test]
    async fn test_decrypt_js_style_gift_wrap() {
        // Simulates exactly what the JS SDK does:
        // 1. Create a signed Nostr event containing the MCP message
        // 2. JSON.stringify that event
        // 3. Encrypt that JSON string in a gift wrap
        let client_keys = Keys::generate();
        let server_keys = Keys::generate();

        let mcp_content = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

        // Step 1: JS SDK creates a signed event (kind 25910 = CTXVM_MESSAGES_KIND)
        let inner_event = EventBuilder::new(Kind::Custom(25910), mcp_content)
            .tag(Tag::public_key(server_keys.public_key()))
            .sign_with_keys(&client_keys)
            .unwrap();

        // Step 2: JSON.stringify the signed event
        let inner_json = serde_json::to_string(&inner_event).unwrap();

        // Step 3: Encrypt as a gift wrap
        let (gift_wrap, _ephemeral) =
            create_simple_gift_wrap(&inner_json, &server_keys.public_key()).await;

        assert_eq!(gift_wrap.kind, Kind::Custom(1059));

        // Decrypt using our function — should get back the inner event JSON
        let decrypted = decrypt_gift_wrap_single_layer(&server_keys, &gift_wrap)
            .await
            .unwrap();

        // Parse the decrypted JSON as a Nostr event
        let parsed: Event = serde_json::from_str(&decrypted).unwrap();
        assert_eq!(parsed.pubkey, client_keys.public_key());
        assert_eq!(parsed.content, mcp_content);
    }

    #[tokio::test]
    async fn test_gift_wrap_roundtrip_single_layer() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();

        // Simulate the full flow: create inner event, stringify, gift wrap, decrypt
        let mcp_content = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let inner_event = EventBuilder::new(Kind::Custom(25910), mcp_content)
            .tag(Tag::public_key(recipient_keys.public_key()))
            .sign_with_keys(&sender_keys)
            .unwrap();
        let inner_json = serde_json::to_string(&inner_event).unwrap();

        // Encrypt (Rust SDK sending)
        let gift_wrap_event =
            gift_wrap_single_layer(&sender_keys, &recipient_keys.public_key(), &inner_json)
                .await
                .unwrap();

        assert_eq!(gift_wrap_event.kind, Kind::Custom(1059));

        // Decrypt
        let decrypted = decrypt_gift_wrap_single_layer(&recipient_keys, &gift_wrap_event)
            .await
            .unwrap();

        let parsed: Event = serde_json::from_str(&decrypted).unwrap();
        assert_eq!(parsed.pubkey, sender_keys.public_key());
        assert_eq!(parsed.content, mcp_content);
    }

    #[tokio::test]
    async fn test_gift_wrap_has_correct_tags() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();

        let gift_wrap_event =
            gift_wrap_single_layer(&sender_keys, &recipient_keys.public_key(), "test")
                .await
                .unwrap();

        // Should have a p tag pointing to the recipient
        let p_tags: Vec<_> = gift_wrap_event
            .tags
            .iter()
            .filter(|t| t.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::P)))
            .collect();
        assert_eq!(p_tags.len(), 1);

        let p_value = p_tags[0].clone().to_vec();
        assert_eq!(p_value[1], recipient_keys.public_key().to_hex());
    }

    #[tokio::test]
    async fn test_gift_wrap_uses_ephemeral_key() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();

        let gift_wrap_event =
            gift_wrap_single_layer(&sender_keys, &recipient_keys.public_key(), "test")
                .await
                .unwrap();

        // The gift wrap event should NOT be signed by the sender's key
        // (it uses an ephemeral key, like the JS SDK)
        assert_ne!(gift_wrap_event.pubkey, sender_keys.public_key());
    }

    /// Regression: gift-wrapped inner events with a tampered pubkey must be
    /// caught by `Event::verify()`.
    #[tokio::test]
    async fn test_forged_inner_event_detected_by_verify() {
        let real_sender = Keys::generate();
        let impersonated = Keys::generate();
        let recipient = Keys::generate();

        let mcp_content = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

        // Step 1: build a legitimately signed inner event
        let inner_event = EventBuilder::new(Kind::Custom(25910), mcp_content)
            .tag(Tag::public_key(recipient.public_key()))
            .sign_with_keys(&real_sender)
            .unwrap();

        // Step 2: tamper the pubkey (keep original, now-invalid, signature)
        let mut forged_json: serde_json::Value = serde_json::to_value(&inner_event).unwrap();
        forged_json["pubkey"] = serde_json::Value::String(impersonated.public_key().to_hex());
        let forged_str = serde_json::to_string(&forged_json).unwrap();

        // Step 3: gift-wrap the forged payload
        let (gift_wrap, _) = create_simple_gift_wrap(&forged_str, &recipient.public_key()).await;

        // Decrypt + parse both succeed — the forgery is syntactically valid
        let decrypted = decrypt_gift_wrap_single_layer(&recipient, &gift_wrap)
            .await
            .unwrap();
        let parsed: Event = serde_json::from_str(&decrypted).unwrap();
        assert_eq!(parsed.pubkey, impersonated.public_key());

        // Signature verification catches the tampered pubkey
        assert!(
            parsed.verify().is_err(),
            "forged inner event must fail signature verification"
        );
    }

    #[tokio::test]
    async fn test_ephemeral_gift_wrap_roundtrip_single_layer() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();

        let mcp_content = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let inner_event = EventBuilder::new(Kind::Custom(25910), mcp_content)
            .tag(Tag::public_key(recipient_keys.public_key()))
            .sign_with_keys(&sender_keys)
            .unwrap();
        let inner_json = serde_json::to_string(&inner_event).unwrap();

        let gift_wrap_event = gift_wrap_single_layer_with_kind(
            &sender_keys,
            &recipient_keys.public_key(),
            &inner_json,
            EPHEMERAL_GIFT_WRAP_KIND,
        )
        .await
        .unwrap();

        assert_eq!(gift_wrap_event.kind, Kind::Custom(EPHEMERAL_GIFT_WRAP_KIND));

        let decrypted = decrypt_gift_wrap_single_layer(&recipient_keys, &gift_wrap_event)
            .await
            .unwrap();
        let parsed: Event = serde_json::from_str(&decrypted).unwrap();
        assert_eq!(parsed.pubkey, sender_keys.public_key());
        assert_eq!(parsed.content, mcp_content);
    }

    #[tokio::test]
    async fn test_invalid_gift_wrap_kind_rejected() {
        let sender_keys = Keys::generate();
        let recipient_keys = Keys::generate();

        let error = gift_wrap_single_layer_with_kind(
            &sender_keys,
            &recipient_keys.public_key(),
            "test",
            4242,
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("Unsupported gift-wrap kind"),
            "unexpected error: {error}"
        );
    }
}
