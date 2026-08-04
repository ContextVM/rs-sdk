//! Conformance tests for the CEP-8 wire format: the `pmi`, `payment_interaction`, and `cap` tag
//! tuples, the `-32602` unsupported-mode negotiation error, and the payment notification params.
//!
//! These pin exact bytes rather than behavior, so a refactor of the builders cannot silently drift
//! the wire format away from the spec and the ts-sdk. Everything here is pure (no relays, no
//! transport), so the file also runs under `--no-default-features`.

use contextvm_sdk::core::constants::tags;
use contextvm_sdk::core::types::{JsonRpcError, PaymentInteractionMode};
use contextvm_sdk::payments::constants::UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE;
use contextvm_sdk::payments::tags::{
    cap_tags_from_priced_capabilities, payment_interaction_tag, pmi_tag, pmi_tags,
};
use contextvm_sdk::{
    PaymentAcceptedParams, PaymentOption, PaymentRejectedParams, PaymentRequiredErrorData,
    PaymentRequiredParams, PricedCapability, UnsupportedPaymentInteractionData,
};
use nostr_sdk::prelude::*;

/// A tag's wire form: the flat string array a relay stores and a peer reads.
fn wire(tag: &Tag) -> Vec<String> {
    tag.clone().to_vec()
}

fn wire_all(list: &[Tag]) -> Vec<Vec<String>> {
    list.iter().map(wire).collect()
}

fn priced(
    method: &str,
    name: &str,
    amount: i64,
    max_amount: Option<i64>,
    unit: &str,
) -> PricedCapability {
    PricedCapability {
        method: method.to_string(),
        name: Some(name.to_string()),
        amount,
        max_amount,
        currency_unit: unit.to_string(),
        description: None,
    }
}

// ── `pmi` tags ───────────────────────────────────────────────────────────────

#[test]
fn cep8_pmi_tag_is_a_two_element_tuple() {
    assert_eq!(
        wire(&pmi_tag("bitcoin-lightning-bolt11")),
        vec!["pmi", "bitcoin-lightning-bolt11"]
    );
    assert_eq!(wire(&pmi_tag("bitcoin-lightning-bolt11"))[0], tags::PMI);
}

#[test]
fn cep8_pmi_tags_emit_one_tag_per_method_in_preference_order() {
    // CEP-8 sends one 2-element `pmi` tag per payment method, in preference order, never a single
    // multi-value tag.
    let list = pmi_tags(&["bitcoin-lightning-bolt11".to_string(), "cashu".to_string()]);
    assert_eq!(
        wire_all(&list),
        vec![
            vec!["pmi", "bitcoin-lightning-bolt11"],
            vec!["pmi", "cashu"],
        ]
    );
}

#[test]
fn cep8_pmi_tags_empty_input_emits_nothing() {
    assert!(pmi_tags(&[]).is_empty());
}

// ── `payment_interaction` tags ───────────────────────────────────────────────

#[test]
fn cep8_payment_interaction_tag_values() {
    assert_eq!(
        wire(&payment_interaction_tag(
            PaymentInteractionMode::ExplicitGating
        )),
        vec!["payment_interaction", "explicit_gating"]
    );
    assert_eq!(
        wire(&payment_interaction_tag(
            PaymentInteractionMode::Transparent
        )),
        vec!["payment_interaction", "transparent"]
    );
    assert_eq!(
        wire(&payment_interaction_tag(
            PaymentInteractionMode::Transparent
        ))[0],
        tags::PAYMENT_INTERACTION
    );
}

// ── `cap` tags ───────────────────────────────────────────────────────────────

#[test]
fn cep8_cap_tag_matches_the_spec_example() {
    let list = cap_tags_from_priced_capabilities(&[priced(
        "tools/call",
        "get_weather",
        100,
        None,
        "sats",
    )]);
    assert_eq!(
        wire_all(&list),
        vec![vec!["cap", "tool:get_weather", "100", "sats"]]
    );
    assert_eq!(wire(&list[0])[0], tags::CAPABILITY);
}

#[test]
fn cep8_cap_tag_identifier_prefixes_by_method() {
    let list = cap_tags_from_priced_capabilities(&[
        priced("tools/call", "add", 10, None, "sats"),
        priced("prompts/get", "summarize", 20, None, "sats"),
        priced("resources/read", "file:///a.txt", 30, None, "sats"),
    ]);
    assert_eq!(
        wire_all(&list),
        vec![
            vec!["cap", "tool:add", "10", "sats"],
            vec!["cap", "prompt:summarize", "20", "sats"],
            vec!["cap", "resource:file:///a.txt", "30", "sats"],
        ]
    );
}

#[test]
fn cep8_cap_tag_range_price_is_hyphenated() {
    let list = cap_tags_from_priced_capabilities(&[priced(
        "tools/call",
        "generate",
        100,
        Some(500),
        "sats",
    )]);
    assert_eq!(
        wire_all(&list),
        vec![vec!["cap", "tool:generate", "100-500", "sats"]]
    );
}

// ── `-32602` unsupported payment_interaction ─────────────────────────────────

#[test]
fn cep8_unsupported_payment_interaction_data_shape() {
    let data = UnsupportedPaymentInteractionData::new(PaymentInteractionMode::ExplicitGating);
    assert_eq!(
        serde_json::to_value(&data).expect("data serializes"),
        serde_json::json!({
            "requested": "explicit_gating",
            "supported": ["transparent"],
        })
    );
}

#[test]
fn cep8_unsupported_payment_interaction_error_object() {
    let error = JsonRpcError {
        code: UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE,
        message: "Unsupported payment_interaction mode: explicit_gating".to_string(),
        data: serde_json::to_value(UnsupportedPaymentInteractionData::new(
            PaymentInteractionMode::ExplicitGating,
        ))
        .ok(),
    };
    assert_eq!(
        serde_json::to_value(&error).expect("error serializes"),
        serde_json::json!({
            "code": -32602,
            "message": "Unsupported payment_interaction mode: explicit_gating",
            "data": {
                "requested": "explicit_gating",
                "supported": ["transparent"],
            },
        })
    );
}

#[test]
fn cep8_unsupported_payment_interaction_code_is_invalid_params() {
    // A negotiation failure is a JSON-RPC invalid-params error, not one of the payment error codes.
    assert_eq!(UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE, -32602);
}

// ── Payment notification params ──────────────────────────────────────────────

#[test]
fn cep8_payment_required_params_shape() {
    let params = PaymentRequiredParams {
        amount: 100,
        pay_req: "lnbc1...".to_string(),
        pmi: "bitcoin-lightning-bolt11".to_string(),
        description: None,
        ttl: None,
        meta: None,
    };
    // Absent optionals are dropped, matching a ts emitter leaving them `undefined`.
    assert_eq!(
        serde_json::to_value(&params).expect("params serialize"),
        serde_json::json!({
            "amount": 100,
            "pay_req": "lnbc1...",
            "pmi": "bitcoin-lightning-bolt11",
        })
    );
}

#[test]
fn cep8_payment_accepted_params_shape() {
    let params = PaymentAcceptedParams {
        amount: 100,
        pmi: "bitcoin-lightning-bolt11".to_string(),
        meta: None,
    };
    assert_eq!(
        serde_json::to_value(&params).expect("params serialize"),
        serde_json::json!({
            "amount": 100,
            "pmi": "bitcoin-lightning-bolt11",
        })
    );
}

#[test]
fn cep8_payment_rejected_params_shape() {
    let params = PaymentRejectedParams {
        pmi: "bitcoin-lightning-bolt11".to_string(),
        amount: Some(100),
        message: Some("insufficient".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&params).expect("params serialize"),
        serde_json::json!({
            "pmi": "bitcoin-lightning-bolt11",
            "amount": 100,
            "message": "insufficient",
        })
    );

    let minimal = PaymentRejectedParams {
        pmi: "cashu".to_string(),
        amount: None,
        message: None,
    };
    assert_eq!(
        serde_json::to_value(&minimal).expect("params serialize"),
        serde_json::json!({ "pmi": "cashu" })
    );
}

#[test]
fn cep8_payment_required_error_data_shape() {
    let data = PaymentRequiredErrorData {
        instructions: None,
        payment_options: vec![PaymentOption {
            amount: 100,
            pmi: "bitcoin-lightning-bolt11".to_string(),
            pay_req: "lnbc1...".to_string(),
            description: None,
            ttl: None,
            meta: None,
        }],
    };
    assert_eq!(
        serde_json::to_value(&data).expect("data serializes"),
        serde_json::json!({
            "payment_options": [{
                "amount": 100,
                "pmi": "bitcoin-lightning-bolt11",
                "pay_req": "lnbc1...",
            }],
        })
    );
}
