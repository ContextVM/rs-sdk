//! CEP-8 tag builders and single-tag parsers (`cap`, `pmi`, `payment_interaction`).
//!
//! Builders follow the repo idiom `Tag::custom(TagKind::Custom(name.into()), ..)`
//! and parsers the `tag.clone().to_vec()` idiom (see
//! `src/transport/discovery_tags.rs`). The multi-value `extract_pmis` /
//! `extract_payment_interaction` collectors that live in `discovery_tags.rs`
//! build on these single-tag primitives.
//!
//! There is deliberately **no `cap` parser** (ts-sdk parity): `cap-tags.ts` is
//! builder-only and a client reads price from `payment_required.amount` /
//! `payment_options`, never from the advertised `cap` tag.

use nostr_sdk::prelude::*;

use crate::core::constants::tags;
use crate::core::types::PaymentInteractionMode;
use crate::payments::types::PricedCapability;

/// CEP-8 `cap` tags from priced capabilities (order preserved). Omits a capability whose `name` is `None`
/// or whose `method` is not one of `tools/call` / `prompts/get` / `resources/read`.
pub fn cap_tags_from_priced_capabilities(caps: &[PricedCapability]) -> Vec<Tag> {
    caps.iter()
        .filter_map(|cap| {
            let identifier = cep8_capability_identifier(cap)?;
            let price = match cap.max_amount {
                Some(max) => format!("{}-{}", cap.amount, max),
                None => cap.amount.to_string(),
            };
            Some(Tag::custom(
                TagKind::Custom(tags::CAPABILITY.into()),
                vec![identifier, price, cap.currency_unit.clone()],
            ))
        })
        .collect()
}

/// Maps a priced capability to its CEP-8 `cap` identifier (`tool:` / `prompt:` / `resource:` prefix),
/// or `None` for an unnamed capability or an unsupported method. An empty-string name is treated as
/// unnamed (mirrors ts `if (!cap.name)`, where `""` is falsy).
fn cep8_capability_identifier(cap: &PricedCapability) -> Option<String> {
    let name = cap.name.as_deref().filter(|n| !n.is_empty())?;
    match cap.method.as_str() {
        "tools/call" => Some(format!("tool:{name}")),
        "prompts/get" => Some(format!("prompt:{name}")),
        "resources/read" => Some(format!("resource:{name}")),
        _ => None,
    }
}

/// A single `pmi` tag.
pub fn pmi_tag(pmi: &str) -> Tag {
    Tag::custom(TagKind::Custom(tags::PMI.into()), vec![pmi.to_string()])
}

/// `pmi` tags in server-preference order. Callers pass each processor's `pmi` as `&[String]`.
pub fn pmi_tags(pmis: &[String]) -> Vec<Tag> {
    pmis.iter().map(|p| pmi_tag(p)).collect()
}

/// A `payment_interaction` tag for the given mode.
pub fn payment_interaction_tag(mode: PaymentInteractionMode) -> Tag {
    let value = match mode {
        PaymentInteractionMode::Transparent => "transparent",
        PaymentInteractionMode::ExplicitGating => "explicit_gating",
    };
    Tag::custom(
        TagKind::Custom(tags::PAYMENT_INTERACTION.into()),
        vec![value.to_string()],
    )
}

/// Parse a `pmi` tag's value (index 1) if `tag` is a `pmi` tag.
pub fn parse_pmi_tag(tag: &Tag) -> Option<String> {
    let parts = tag.clone().to_vec();
    match (parts.first().map(String::as_str), parts.get(1)) {
        (Some(tags::PMI), Some(v)) => Some(v.clone()),
        _ => None,
    }
}

/// Parse a `payment_interaction` tag into a mode; unknown values yield `None`.
pub fn parse_payment_interaction_tag(tag: &Tag) -> Option<PaymentInteractionMode> {
    let parts = tag.clone().to_vec();
    if parts.first().map(String::as_str) != Some(tags::PAYMENT_INTERACTION) {
        return None;
    }
    match parts.get(1).map(String::as_str) {
        Some("transparent") => Some(PaymentInteractionMode::Transparent),
        Some("explicit_gating") => Some(PaymentInteractionMode::ExplicitGating),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(
        method: &str,
        name: Option<&str>,
        amount: i64,
        max_amount: Option<i64>,
        unit: &str,
    ) -> PricedCapability {
        PricedCapability {
            method: method.to_string(),
            name: name.map(String::from),
            amount,
            max_amount,
            currency_unit: unit.to_string(),
            description: None,
        }
    }

    fn parts(tag: &Tag) -> Vec<String> {
        tag.clone().to_vec()
    }

    fn all_parts(tags: &[Tag]) -> Vec<Vec<String>> {
        tags.iter().map(parts).collect()
    }

    // ── cap builder (exact cap-tags.test.ts vectors) ────────────────

    #[test]
    fn cap_tags_build_tool_prompt_resource_in_order() {
        let caps = vec![
            priced("tools/call", Some("add"), 1, None, "sats"),
            priced("prompts/get", Some("welcome"), 2, None, "sats"),
            priced("resources/read", Some("greeting://alice"), 3, None, "sats"),
        ];
        assert_eq!(
            all_parts(&cap_tags_from_priced_capabilities(&caps)),
            vec![
                vec!["cap", "tool:add", "1", "sats"],
                vec!["cap", "prompt:welcome", "2", "sats"],
                vec!["cap", "resource:greeting://alice", "3", "sats"],
            ]
        );
    }

    #[test]
    fn cap_tags_use_range_when_max_amount_present() {
        let caps = vec![priced("tools/call", Some("add"), 100, Some(1000), "sats")];
        assert_eq!(
            all_parts(&cap_tags_from_priced_capabilities(&caps)),
            vec![vec!["cap", "tool:add", "100-1000", "sats"]]
        );
    }

    #[test]
    fn cap_tags_skip_unnamed_empty_name_and_unsupported_methods() {
        let caps = vec![
            priced("tools/call", None, 1, None, "sats"), // no name
            priced("tools/call", Some(""), 1, None, "sats"), // empty name (ts `!cap.name`)
            priced("resources/list", Some("ignored"), 1, None, "sats"), // unsupported method
        ];
        assert!(cap_tags_from_priced_capabilities(&caps).is_empty());
    }

    #[test]
    fn cap_tags_preserve_order_across_skips() {
        let caps = vec![
            priced("tools/call", Some("a"), 1, None, "sats"),
            priced("tools/call", None, 9, None, "sats"), // skipped (unnamed)
            priced("prompts/get", Some("b"), 2, None, "sats"),
        ];
        assert_eq!(
            all_parts(&cap_tags_from_priced_capabilities(&caps)),
            vec![
                vec!["cap", "tool:a", "1", "sats"],
                vec!["cap", "prompt:b", "2", "sats"],
            ]
        );
    }

    // ── pmi / payment_interaction builders ──────────────────────────

    #[test]
    fn pmi_tags_preserve_input_order() {
        let pmis = vec!["one".to_string(), "two".to_string()];
        assert_eq!(
            all_parts(&pmi_tags(&pmis)),
            vec![vec!["pmi", "one"], vec!["pmi", "two"]]
        );
    }

    #[test]
    fn payment_interaction_tag_renders_both_modes() {
        assert_eq!(
            parts(&payment_interaction_tag(
                PaymentInteractionMode::Transparent
            )),
            vec!["payment_interaction", "transparent"]
        );
        assert_eq!(
            parts(&payment_interaction_tag(
                PaymentInteractionMode::ExplicitGating
            )),
            vec!["payment_interaction", "explicit_gating"]
        );
    }

    // ── parsers ─────────────────────────────────────────────────────

    #[test]
    fn parse_pmi_tag_roundtrips_builder() {
        let tag = pmi_tag("bitcoin-lightning-bolt11");
        assert_eq!(
            parse_pmi_tag(&tag),
            Some("bitcoin-lightning-bolt11".to_string())
        );
    }

    #[test]
    fn parse_pmi_tag_rejects_non_pmi_tag() {
        let tag = payment_interaction_tag(PaymentInteractionMode::Transparent);
        assert_eq!(parse_pmi_tag(&tag), None);
    }

    #[test]
    fn parse_payment_interaction_tag_roundtrips_both_modes() {
        for mode in [
            PaymentInteractionMode::Transparent,
            PaymentInteractionMode::ExplicitGating,
        ] {
            assert_eq!(
                parse_payment_interaction_tag(&payment_interaction_tag(mode)),
                Some(mode)
            );
        }
    }

    #[test]
    fn parse_payment_interaction_tag_unknown_value_is_none() {
        let tag = Tag::custom(
            TagKind::Custom(tags::PAYMENT_INTERACTION.into()),
            vec!["bogus".to_string()],
        );
        assert_eq!(parse_payment_interaction_tag(&tag), None);
    }

    #[test]
    fn parse_payment_interaction_tag_rejects_non_matching_tag() {
        assert_eq!(parse_payment_interaction_tag(&pmi_tag("x")), None);
    }
}
