//! Pure-function tests for the crypto, key, and encoding surface.
//!
//! These exercise `cvm_encrypt_nip44` / `cvm_decrypt_nip44`, secret-key
//! restore, and npub conversion directly — no relay required.

use contextvm_ffi::{
    cvm_decrypt_nip44, cvm_encrypt_nip44, cvm_error_code, cvm_error_free, cvm_error_message,
    cvm_keys_free, cvm_keys_from_secret_key, cvm_keys_generate, cvm_keys_public_key,
    cvm_keys_secret_key, cvm_pubkey_hex_to_npub, cvm_string_free,
    error::{ErrorCode, FfiError},
};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

fn to_c_str(s: &str) -> *mut c_char {
    CString::new(s).unwrap().into_raw()
}

unsafe fn from_c_str(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    CStr::from_ptr(ptr).to_str().unwrap_or("").to_string()
}

/// NIP-44 encrypt then decrypt must round-trip between two keypairs.
#[test]
fn test_encrypt_decrypt_roundtrip() {
    let mut error: *mut FfiError = ptr::null_mut();

    let alice = cvm_keys_generate(&mut error);
    let bob = cvm_keys_generate(&mut error);
    assert!(alice.id > 0 && bob.id > 0);

    let bob_pub = cvm_keys_public_key(bob, &mut error);
    let bob_pub = unsafe { from_c_str(bob_pub) };
    assert_eq!(bob_pub.len(), 64);

    let plaintext = "the quick brown fox 🦊";
    let ciphertext = cvm_encrypt_nip44(alice, to_c_str(&bob_pub), to_c_str(plaintext), &mut error);
    assert!(!ciphertext.is_null(), "encrypt must succeed");

    let ciphertext = unsafe { from_c_str(ciphertext) };
    assert!(!ciphertext.is_empty(), "ciphertext must be non-empty");
    assert_ne!(
        ciphertext, plaintext,
        "ciphertext must differ from plaintext"
    );

    // Decrypt with the recipient keys; sender is alice's pubkey.
    let alice_pub = cvm_keys_public_key(alice, &mut error);
    let alice_pub = unsafe { from_c_str(alice_pub) };

    let recovered = cvm_decrypt_nip44(bob, to_c_str(&alice_pub), to_c_str(&ciphertext), &mut error);
    assert!(!recovered.is_null(), "decrypt must succeed");
    assert_eq!(
        unsafe { from_c_str(recovered) },
        plaintext,
        "roundtrip must recover plaintext"
    );

    // Cleanup
    cvm_string_free(recovered);
    cvm_keys_free(alice);
    cvm_keys_free(bob);
}

/// Encrypting to a bogus recipient pubkey must report a validation error,
/// not crash or return garbage.
#[test]
fn test_encrypt_rejects_invalid_recipient() {
    let mut error: *mut FfiError = ptr::null_mut();
    let keys = cvm_keys_generate(&mut error);

    let result = cvm_encrypt_nip44(keys, to_c_str("not-a-pubkey"), to_c_str("hi"), &mut error);

    assert!(result.is_null(), "encrypt with bad pubkey must return null");
    assert!(!error.is_null(), "an error must be reported");
    assert_eq!(unsafe { (*error).code }, ErrorCode::Validation);

    cvm_error_free(error);
    cvm_keys_free(keys);
}

/// Calling crypto with an invalid keys handle must fail cleanly.
#[test]
fn test_encrypt_rejects_invalid_keys_handle() {
    let mut error: *mut FfiError = ptr::null_mut();
    let bogus = contextvm_ffi::handle::FfiHandle { id: 999_999 };

    let result = cvm_encrypt_nip44(
        bogus,
        to_c_str("0000000000000000000000000000000000000000000000000000000000000001"),
        to_c_str("hi"),
        &mut error,
    );

    assert!(result.is_null());
    assert!(!error.is_null());
    assert_eq!(unsafe { (*error).code }, ErrorCode::Other);
    cvm_error_free(error);
}

/// A secret key exported from one keypair must restore to the same pubkey.
#[test]
fn test_secret_key_restore_roundtrip() {
    let mut error: *mut FfiError = ptr::null_mut();

    let original = cvm_keys_generate(&mut error);
    let original_pub = cvm_keys_public_key(original, &mut error);
    let original_pub = unsafe { from_c_str(original_pub) };

    let secret = cvm_keys_secret_key(original, &mut error);
    let secret = unsafe { from_c_str(secret) };
    assert_eq!(secret.len(), 64, "secret key hex is 64 chars");

    let restored = cvm_keys_from_secret_key(to_c_str(&secret), &mut error);
    assert!(restored.id > 0, "restore must produce a valid handle");

    let restored_pub = cvm_keys_public_key(restored, &mut error);
    assert_eq!(
        unsafe { from_c_str(restored_pub) },
        original_pub,
        "restored pubkey must match original"
    );

    cvm_keys_free(original);
    cvm_keys_free(restored);
}

/// `cvm_keys_from_secret_key` must reject malformed input.
#[test]
fn test_from_secret_key_rejects_garbage() {
    let mut error: *mut FfiError = ptr::null_mut();
    let handle = cvm_keys_from_secret_key(to_c_str("definitely-not-a-key"), &mut error);

    assert_eq!(handle.id, 0, "no handle for garbage secret key");
    assert!(!error.is_null());
    cvm_error_free(error);
}

/// A hex pubkey must convert to a bech32 npub.
#[test]
fn test_pubkey_hex_to_npub() {
    let mut error: *mut FfiError = ptr::null_mut();
    let keys = cvm_keys_generate(&mut error);
    let hex = cvm_keys_public_key(keys, &mut error);
    let hex = unsafe { from_c_str(hex) };

    let npub = cvm_pubkey_hex_to_npub(to_c_str(&hex), &mut error);
    assert!(!npub.is_null(), "npub conversion must succeed");
    let npub_str = unsafe { from_c_str(npub) };
    assert!(
        npub_str.starts_with("npub1"),
        "npub must have bech32 prefix: {npub_str}"
    );

    cvm_string_free(npub);
    cvm_keys_free(keys);
}

/// `cvm_pubkey_hex_to_npub` must reject bad hex.
#[test]
fn test_pubkey_hex_to_npub_rejects_bad_hex() {
    let mut error: *mut FfiError = ptr::null_mut();
    let result = cvm_pubkey_hex_to_npub(to_c_str("zz"), &mut error);

    assert!(result.is_null());
    assert!(!error.is_null());
    assert_eq!(unsafe { (*error).code }, ErrorCode::Validation);
    cvm_error_free(error);
}

/// `cvm_error_code` / `cvm_error_message` must read a populated error and
/// tolerate a null pointer.
#[test]
fn test_error_accessors() {
    // Null pointer tolerance.
    assert_eq!(cvm_error_code(ptr::null()), ErrorCode::Ok);

    // Populated error: borrow an error from a known-failing call.
    let mut error: *mut FfiError = ptr::null_mut();
    let _ = cvm_keys_from_secret_key(to_c_str("garbage"), &mut error);
    assert!(!error.is_null());
    assert_eq!(cvm_error_code(error), ErrorCode::Other);
    let msg = cvm_error_message(error);
    assert!(!unsafe { from_c_str(msg) }.is_empty());
    cvm_string_free(msg);
    cvm_error_free(error);
}
