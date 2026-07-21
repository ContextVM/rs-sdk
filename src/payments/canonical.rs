//! CEP-8 canonical invocation identity for explicit-gating authorization matching.
//!
//! Identity = the requesting client pubkey plus the SHA-256 digest of the RFC 8785 (JCS)
//! serialization of `{ method, params }` with the top-level `params._meta` removed. `_meta`
//! is MCP's reserved per-request namespace (progressToken, stream, ...) and is regenerated
//! on every call, so excluding it lets a retry match a paid authorization. See CEP-8.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::payments::errors::PaymentError;

/// Client pubkey plus the canonical invocation hash. Internal correlation value (never on the
/// wire); the authorization store keys on the derived string `"{client_pubkey}:{invocation_hash}"`,
/// so this bundle needs no `Hash` (it is not itself a map key). `PartialEq`/`Eq` support test
/// assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalInvocationIdentity {
    /// Requesting client public key, hex.
    pub client_pubkey: String,
    /// Lowercase hex SHA-256 of `JCS({ method, params })` with top-level `params._meta` removed.
    pub invocation_hash: String,
}

/// Compute the canonical invocation hash for `method` + semantic `params`.
///
/// `params` maps the JSON-RPC params slot: `None` means the request carried no `params` (the key
/// is omitted from the canonical object), while `Some(Value::Null)` means an explicit `null` (the
/// key is kept as `null`). Only an object `params` carries a reserved `_meta`, which is removed at
/// the top level; arrays and primitives pass through unchanged.
///
/// Returns [`PaymentError::Canonicalize`] if the payload cannot be canonicalized. Because a
/// `serde_json::Value` is always valid JSON, the only trigger is an integer outside the JSON safe
/// range (`|n| > 2^53 - 1`), which RFC 8785 / I-JSON deem non-interoperable; failing closed there
/// never yields a wrong hash.
pub fn compute_canonical_invocation_hash(
    method: &str,
    params: Option<&Value>,
) -> Result<String, PaymentError> {
    let canonical = canonical_payload_string(method, params)?;
    Ok(hex::encode(Sha256::digest(canonical.as_bytes())))
}

/// Compute the full canonical invocation identity (`client_pubkey` + `invocation_hash`).
///
/// Fallible for the same reason as [`compute_canonical_invocation_hash`]; the hash is computed
/// first and any error propagates.
pub fn compute_canonical_invocation_identity(
    client_pubkey: &str,
    method: &str,
    params: Option<&Value>,
) -> Result<CanonicalInvocationIdentity, PaymentError> {
    Ok(CanonicalInvocationIdentity {
        client_pubkey: client_pubkey.to_string(),
        invocation_hash: compute_canonical_invocation_hash(method, params)?,
    })
}

/// Build the JCS canonical string of `{ method, params }` with top-level `params._meta` removed.
/// Separated from the hash step so tests can assert the canonical bytes directly (byte-level
/// diagnostics on a parity failure).
fn canonical_payload_string(method: &str, params: Option<&Value>) -> Result<String, PaymentError> {
    let semantic: Option<Value> = params.map(|p| match p {
        Value::Object(map) if map.contains_key("_meta") => {
            let mut stripped = map.clone();
            stripped.remove("_meta"); // shallow, top-level only (nested `_meta` is semantic)
            Value::Object(stripped)
        }
        other => other.clone(),
    });

    let mut payload = Map::new();
    payload.insert("method".to_string(), Value::String(method.to_string()));
    if let Some(sp) = semantic {
        payload.insert("params".to_string(), sp);
    }
    let payload = Value::Object(payload);

    // Reject any integer outside the JSON safe range BEFORE serializing. Two reasons:
    // (1) RFC 8785 / I-JSON treat such integers as non-interoperable, and a JS peer parses
    //     them lossily, so a shared byte form is not defined; (2) the JCS engine's own bounds
    //     check computes `i64::abs`, which overflows on `i64::MIN` (a debug panic that bypasses
    //     the `?` below, and a release miscanonicalization that would silently diverge). This
    //     pre-scan uses comparisons only, so it is panic-free and fails closed on every sign.
    if contains_unsafe_json_integer(&payload) {
        return Err(PaymentError::Canonicalize {
            method: method.to_string(),
        });
    }

    json_canon::to_string(&payload).map_err(|_| PaymentError::Canonicalize {
        method: method.to_string(),
    })
}

/// True if `value` contains a JSON integer whose magnitude exceeds the IEEE-754 safe range
/// (`|n| > 2^53 - 1`), scanned recursively. Uses a bounded range check, never `i64::abs`
/// (which overflows on `i64::MIN`). Floats pass through: they serialize via the ECMAScript
/// number algorithm and match the reference implementation regardless of magnitude.
fn contains_unsafe_json_integer(value: &Value) -> bool {
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991; // 2^53 - 1
    match value {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&i)
            } else if let Some(u) = n.as_u64() {
                u > MAX_SAFE_INTEGER as u64
            } else {
                false // a float; the ryu-js path matches JS
            }
        }
        Value::Array(items) => items.iter().any(contains_unsafe_json_integer),
        Value::Object(map) => map.values().any(contains_unsafe_json_integer),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    //! The ts-sdk's circular-reference / function / symbol / BigInt "throws" tests have
    //! no analog here: a `serde_json::Value` cannot represent any of those (nor
    //! `NaN`/`Infinity`), so the only failure surface is the safe-integer guard exercised
    //! by `unsafe_integers_fail_closed_never_panic`.

    use super::{
        canonical_payload_string, compute_canonical_invocation_hash,
        compute_canonical_invocation_identity,
    };
    use crate::payments::errors::PaymentError;
    use serde_json::{json, Value};

    /// The committed cross-SDK byte-parity anchor, embedded at compile time so a fixture
    /// change forces a recompile.
    const FIXTURE: &str = include_str!("../../tests/fixtures/canonical_vectors.json");

    #[derive(serde::Deserialize)]
    struct Fixture {
        vectors: Vec<Vector>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Vector {
        name: String,
        method: String,
        /// Raw JSON text of the params. Absent means JSON-RPC `undefined` (no params key).
        #[serde(default)]
        params_text: Option<String>,
        expected_canonical: String,
        expected_hash: String,
    }

    fn params_of(v: &Vector) -> Option<Value> {
        v.params_text
            .as_ref()
            .map(|t| serde_json::from_str(t).expect("fixture paramsText must be valid JSON"))
    }

    fn hash(method: &str, params: Option<&Value>) -> String {
        compute_canonical_invocation_hash(method, params).expect("hash should succeed")
    }

    #[test]
    fn golden_vectors_conformance() {
        let fixture: Fixture = serde_json::from_str(FIXTURE).expect("fixture parses");
        assert!(!fixture.vectors.is_empty(), "fixture must carry vectors");
        for v in &fixture.vectors {
            let params = params_of(v);
            let canonical = canonical_payload_string(&v.method, params.as_ref())
                .unwrap_or_else(|e| panic!("vector '{}' canonicalization failed: {e}", v.name));
            assert_eq!(
                canonical, v.expected_canonical,
                "canonical string mismatch for vector '{}'",
                v.name
            );
            let digest = compute_canonical_invocation_hash(&v.method, params.as_ref())
                .unwrap_or_else(|e| panic!("vector '{}' hashing failed: {e}", v.name));
            assert_eq!(
                digest, v.expected_hash,
                "hash mismatch for vector '{}'",
                v.name
            );
        }
    }

    #[test]
    fn top_level_meta_excluded_but_semantics_kept() {
        let base = json!({ "name": "add", "arguments": { "a": 1, "b": 2 } });
        let with_meta = json!({ "name": "add", "arguments": { "a": 1, "b": 2 }, "_meta": { "progressToken": 1 } });
        let with_meta_multi = json!({
            "name": "add",
            "arguments": { "a": 1, "b": 2 },
            "_meta": { "progressToken": 999, "stream": "writer" }
        });
        assert_eq!(
            hash("tools/call", Some(&base)),
            hash("tools/call", Some(&with_meta))
        );
        assert_eq!(
            hash("tools/call", Some(&base)),
            hash("tools/call", Some(&with_meta_multi))
        );

        // A different semantic arg still differentiates, even with `_meta` present.
        let different_args = json!({ "name": "add", "arguments": { "a": 2, "b": 2 }, "_meta": { "progressToken": 1 } });
        assert_ne!(
            hash("tools/call", Some(&base)),
            hash("tools/call", Some(&different_args))
        );
    }

    #[test]
    fn nested_meta_is_not_stripped() {
        let with_nested = json!({ "arguments": { "_meta": { "x": 1 }, "v": 2 } });
        let without = json!({ "arguments": { "v": 2 } });
        assert_ne!(
            hash("tools/call", Some(&with_nested)),
            hash("tools/call", Some(&without))
        );
    }

    #[test]
    fn none_and_null_params_differ() {
        // `None` omits the params key; `Some(Value::Null)` keeps `params: null`.
        assert_ne!(
            hash("tools/call", None),
            hash("tools/call", Some(&Value::Null))
        );
    }

    #[test]
    fn method_and_value_changes_change_the_hash() {
        let params = json!({ "a": 1 });
        assert_ne!(
            hash("tools/call", Some(&params)),
            hash("prompts/get", Some(&params))
        );
        assert_ne!(
            hash("tools/call", Some(&json!({ "a": 1 }))),
            hash("tools/call", Some(&json!({ "a": 2 })))
        );
    }

    #[test]
    fn key_order_does_not_affect_the_hash() {
        // serde_json normalizes object key order on parse (no `preserve_order` feature),
        // so this pins the end-to-end invariant; json-canon's own UTF-16 re-sort (which
        // diverges from serde_json's code-point order) is exercised by the non-BMP golden
        // vector.
        let a: Value = serde_json::from_str(r#"{"a":1,"b":2,"name":"test"}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"name":"test","b":2,"a":1}"#).unwrap();
        assert_eq!(hash("tools/call", Some(&a)), hash("tools/call", Some(&b)));
    }

    #[test]
    fn array_and_primitive_params_hash_to_lowercase_hex() {
        for params in [json!([1, 2, 3]), json!("literal"), json!(42), json!(true)] {
            let digest = hash("tools/call", Some(&params));
            assert_eq!(digest.len(), 64);
            assert!(digest
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        }
    }

    #[test]
    fn number_edges_render_in_ecmascript_form() {
        let canonical = |v: Value| canonical_payload_string("tools/call", Some(&v)).unwrap();
        assert_eq!(
            canonical(json!({ "n": 1e21 })),
            r#"{"method":"tools/call","params":{"n":1e+21}}"#
        );
        assert_eq!(
            canonical(json!({ "n": 1e-7 })),
            r#"{"method":"tools/call","params":{"n":1e-7}}"#
        );
        // Negative zero renders as `0`.
        assert_eq!(
            canonical(json!({ "n": -0.0 })),
            r#"{"method":"tools/call","params":{"n":0}}"#
        );
        // A whole-valued float renders without a fractional part.
        assert_eq!(
            canonical(json!({ "n": 1.0 })),
            r#"{"method":"tools/call","params":{"n":1}}"#
        );
        // The safe-integer boundary is preserved exactly.
        assert_eq!(
            canonical(json!({ "n": 9_007_199_254_740_991_i64 })),
            r#"{"method":"tools/call","params":{"n":9007199254740991}}"#
        );
    }

    #[test]
    fn unsafe_integers_fail_closed_never_panic() {
        // The exact safe boundary, both signs, must succeed.
        assert!(compute_canonical_invocation_hash(
            "tools/call",
            Some(&json!({ "n": 9_007_199_254_740_991_i64 }))
        )
        .is_ok());
        assert!(compute_canonical_invocation_hash(
            "tools/call",
            Some(&json!({ "n": -9_007_199_254_740_991_i64 }))
        )
        .is_ok());

        // Everything past the safe range must return Err(Canonicalize): never Ok, never a
        // panic (this test runs in the default debug profile, where json-canon's own
        // i64::abs bounds check would panic on i64::MIN without the pre-scan).
        let unsafe_values = [
            json!({ "n": 9_007_199_254_740_992_i64 }),  // 2^53
            json!({ "n": -9_007_199_254_740_992_i64 }), // -2^53
            json!({ "n": 9_007_199_254_740_993_i64 }),  // 2^53 + 1
            json!({ "n": i64::MAX }),
            json!({ "n": i64::MIN }), // json-canon's abs-overflow case
            json!({ "n": u64::MAX }), // above i64::MAX, stays u64
            json!({ "n": 9_007_199_254_740_993_u64 }), // fits i64 but unsafe
            json!({ "a": { "b": [1, i64::MIN] } }), // nested, proves recursion
            json!([1, 2, i64::MIN]),  // array params at the top level
        ];
        for v in &unsafe_values {
            let result = compute_canonical_invocation_hash("tools/call", Some(v));
            assert!(
                matches!(&result, Err(PaymentError::Canonicalize { .. })),
                "expected Canonicalize error for {v}, got {result:?}"
            );
        }

        // The u64 cases must stay integer-typed (PosInt); a coercion to f64 would bypass
        // the integer guard entirely.
        assert!(json!({ "n": u64::MAX })["n"].is_u64());
        let fits_i64 = json!({ "n": 9_007_199_254_740_993_u64 });
        assert!(fits_i64["n"].is_u64() || fits_i64["n"].is_i64());
    }

    #[test]
    fn canonicalize_error_display_matches_ts_message() {
        let err = compute_canonical_invocation_hash("tools/call", Some(&json!({ "n": i64::MIN })))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Failed to canonicalize invocation payload for method 'tools/call'. Ensure params contain only JSON-serializable values (no circular references, functions, symbols, or BigInt)."
        );
    }

    #[test]
    fn identity_bundles_pubkey_and_hash() {
        let pubkey = "test-client-pubkey";
        let method = "tools/call";
        let params = json!({ "name": "test" });

        let identity =
            compute_canonical_invocation_identity(pubkey, method, Some(&params)).unwrap();
        assert_eq!(identity.client_pubkey, pubkey);
        assert_eq!(
            identity.invocation_hash,
            compute_canonical_invocation_hash(method, Some(&params)).unwrap()
        );

        // The identity function propagates the hash error on unsafe input.
        let err =
            compute_canonical_invocation_identity(pubkey, method, Some(&json!({ "n": i64::MIN })));
        assert!(matches!(err, Err(PaymentError::Canonicalize { .. })));
    }
}
