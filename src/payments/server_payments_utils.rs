//! CEP-8 server-side payment helpers shared by the payment middlewares:
//! processor-map construction, priced-capability matching, verification-timeout
//! arithmetic, PMI selection, and the resolve-and-initiate step that turns a
//! priced request into a rejection, a waiver, or a payment request.
//!
//! Split from the transparent middleware for the same reason the reference
//! implementation splits it: the explicit-gating middleware consumes the same
//! helpers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::core::types::JsonRpcRequest;
use crate::payments::errors::PaymentError;
use crate::payments::server_payments::ServerPaymentsOptions;
use crate::payments::traits::PaymentProcessor;
use crate::payments::types::{
    Meta, PaymentProcessorCreateParams, PaymentRequiredParams, PricedCapability,
    ResolvePriceParams, ResolvePriceResult,
};

const LOG_TARGET: &str = "contextvm_sdk::payments::server_payments";

/// Default verification timeout when the payment request carries no usable `ttl`: five minutes.
const DEFAULT_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Build the PMI-to-processor map, warning once per duplicate PMI (the last
/// registration wins, exactly as a map insert does).
pub(crate) fn build_processors_by_pmi(
    processors: &[Arc<dyn PaymentProcessor>],
) -> HashMap<String, Arc<dyn PaymentProcessor>> {
    let mut map: HashMap<String, Arc<dyn PaymentProcessor>> = HashMap::new();
    for processor in processors {
        if map
            .insert(processor.pmi().to_string(), Arc::clone(processor))
            .is_some()
        {
            tracing::warn!(
                target: LOG_TARGET,
                pmi = %processor.pmi(),
                "duplicate PMI processor registered, last one wins"
            );
        }
    }
    map
}

/// The first priced-capability entry matching this request, if any.
///
/// An entry matches when its `method` equals the request method and its `name`
/// is either `None` (which prices every invocation of the method) or equal to
/// the capability name the method addresses.
pub(crate) fn match_priced_capability<'a>(
    request: &JsonRpcRequest,
    priced: &'a [PricedCapability],
) -> Option<&'a PricedCapability> {
    let capability_name = capability_name_for_pricing(request);
    priced.iter().find(|p| {
        if p.method != request.method {
            return false;
        }
        match &p.name {
            None => true,
            Some(name) => Some(name.as_str()) == capability_name.as_deref(),
        }
    })
}

/// The name a pricing entry addresses for this request: `params.name` for
/// `tools/call` and `prompts/get`, `params.uri` for `resources/read`, `None`
/// for every other method (and for a non-string value).
pub(crate) fn capability_name_for_pricing(request: &JsonRpcRequest) -> Option<String> {
    let params = request.params.as_ref()?;
    let key = match request.method.as_str() {
        "tools/call" | "prompts/get" => "name",
        "resources/read" => "uri",
        _ => return None,
    };
    params.get(key)?.as_str().map(str::to_string)
}

/// The verification timeout derived from a payment request's `ttl` (seconds).
///
/// No `ttl` and a zero `ttl` both give the five-minute default; anything else
/// converts to a duration, falling back to the default if the multiplication
/// overflows.
pub(crate) fn verification_timeout(ttl_seconds: Option<u64>) -> Duration {
    match ttl_seconds {
        None | Some(0) => DEFAULT_VERIFICATION_TIMEOUT,
        Some(ttl) => match ttl.checked_mul(1000) {
            Some(ms) => Duration::from_millis(ms),
            None => DEFAULT_VERIFICATION_TIMEOUT,
        },
    }
}

/// Select the processor for this request: the first client-advertised PMI that
/// has a processor wins; otherwise (including a client list that matches
/// nothing) the first configured processor; an empty processor list is a
/// configuration error.
pub(crate) fn resolve_payment_processor(
    client_pmis: Option<&[String]>,
    processors_by_pmi: &HashMap<String, Arc<dyn PaymentProcessor>>,
    processors: &[Arc<dyn PaymentProcessor>],
) -> Result<Arc<dyn PaymentProcessor>, PaymentError> {
    let chosen = client_pmis
        .and_then(|pmis| pmis.iter().find_map(|pmi| processors_by_pmi.get(pmi)))
        .or_else(|| processors.first());
    chosen
        .map(Arc::clone)
        .ok_or_else(|| PaymentError::Processor("no payment processors configured".to_string()))
}

/// The outcome of the resolve-and-initiate step for one priced request.
pub(crate) enum InitiationOutcome {
    /// The pricing callback refused service: emit `payment_rejected` and drop.
    Rejected {
        /// The selected processor's PMI (selection happens before pricing, so a
        /// rejection can name one).
        pmi: String,
        /// The capability's listed amount (the rejection reports the listed
        /// price, not a resolved quote).
        amount: i64,
        /// Optional human-readable refusal reason from the callback.
        message: Option<String>,
    },
    /// The pricing callback waived payment: forward untouched, emit nothing.
    Waived,
    /// A payment request was issued: emit `payment_required` and verify.
    PaymentRequired {
        /// The processor that issued (and will verify) the payment.
        processor: Arc<dyn PaymentProcessor>,
        /// The processor-returned payment request, published verbatim.
        payment_required: PaymentRequiredParams,
        /// Quote metadata merged over processor metadata; `None` only when both
        /// are absent.
        merged_meta: Option<Meta>,
        /// Verification timeout derived from the payment request's `ttl`.
        verify_timeout: Duration,
    },
}

/// Select a processor, resolve the price (static or via the configured
/// callback), and either report a rejection / waiver or create the payment
/// request.
///
/// A callback `Err` is a server-side failure and propagates as `Err`; a
/// [`ResolvePriceResult::Reject`] is a business decision and returns
/// [`InitiationOutcome::Rejected`]. The processor is selected before the price
/// resolves so a rejection can name a PMI.
pub(crate) async fn resolve_and_initiate_payment(
    request: &JsonRpcRequest,
    priced: &PricedCapability,
    request_event_id: &str,
    client_pubkey: &str,
    client_pmis: Option<&[String]>,
    options: &ServerPaymentsOptions,
    processors_by_pmi: &HashMap<String, Arc<dyn PaymentProcessor>>,
) -> Result<InitiationOutcome, PaymentError> {
    let processor = resolve_payment_processor(client_pmis, processors_by_pmi, &options.processors)?;

    let quote = match &options.resolve_price {
        Some(resolver) => {
            resolver
                .resolve_price(ResolvePriceParams {
                    capability: priced.clone(),
                    request: request.clone(),
                    client_pubkey: client_pubkey.to_string(),
                    request_event_id: request_event_id.to_string(),
                })
                .await?
        }
        None => ResolvePriceResult::Quote {
            amount: priced.amount,
            description: priced.description.clone(),
            meta: None,
        },
    };

    let (amount, description, quote_meta) = match quote {
        ResolvePriceResult::Reject { message } => {
            return Ok(InitiationOutcome::Rejected {
                pmi: processor.pmi().to_string(),
                amount: priced.amount,
                message,
            });
        }
        // The waiver's metadata is dropped: the waive path emits no
        // notification at all, matching the reference implementation's
        // behavior (its docs promise more than its emitter delivers).
        ResolvePriceResult::Waive { meta: _ } => return Ok(InitiationOutcome::Waived),
        ResolvePriceResult::Quote {
            amount,
            description,
            meta,
        } => (amount, description, meta),
    };

    let payment_required = processor
        .create_payment_required(PaymentProcessorCreateParams {
            amount,
            description,
            request_event_id: request_event_id.to_string(),
            client_pubkey: client_pubkey.to_string(),
        })
        .await?;

    // Quote metadata overrides processor metadata key-by-key; `None` only when
    // both are absent.
    let merged_meta = match (payment_required.meta.clone(), quote_meta) {
        (None, None) => None,
        (processor_meta, quote_meta) => {
            let mut merged = processor_meta.unwrap_or_default();
            if let Some(quote_meta) = quote_meta {
                for (key, value) in quote_meta {
                    merged.insert(key, value);
                }
            }
            Some(merged)
        }
    };

    let verify_timeout = verification_timeout(payment_required.ttl);

    Ok(InitiationOutcome::PaymentRequired {
        processor,
        payment_required,
        merged_meta,
        verify_timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payments::traits::ResolvePrice;
    use crate::payments::types::{PaymentProcessorVerifyParams, VerifyOutcome};
    use async_trait::async_trait;

    /// A minimal local processor double: a fixed PMI, a canned payment request.
    /// Local to this module so these tests run in every feature configuration.
    struct StubProcessor {
        pmi: String,
        ttl: Option<u64>,
        meta: Option<Meta>,
        fail_create: bool,
    }

    impl StubProcessor {
        fn new(pmi: &str) -> Self {
            Self {
                pmi: pmi.to_string(),
                ttl: None,
                meta: None,
                fail_create: false,
            }
        }
    }

    #[async_trait]
    impl PaymentProcessor for StubProcessor {
        fn pmi(&self) -> &str {
            &self.pmi
        }

        async fn create_payment_required(
            &self,
            params: PaymentProcessorCreateParams,
        ) -> Result<PaymentRequiredParams, PaymentError> {
            if self.fail_create {
                return Err(PaymentError::Processor("create failed".to_string()));
            }
            Ok(PaymentRequiredParams {
                amount: params.amount,
                pay_req: format!("invoice-{}", params.request_event_id),
                pmi: self.pmi.clone(),
                description: params.description,
                ttl: self.ttl,
                meta: self.meta.clone(),
            })
        }

        async fn verify_payment(
            &self,
            _params: PaymentProcessorVerifyParams,
        ) -> Result<VerifyOutcome, PaymentError> {
            Ok(VerifyOutcome::default())
        }
    }

    struct StaticResolver(ResolvePriceResult);
    #[async_trait]
    impl ResolvePrice for StaticResolver {
        async fn resolve_price(
            &self,
            _params: ResolvePriceParams,
        ) -> Result<ResolvePriceResult, PaymentError> {
            Ok(self.0.clone())
        }
    }

    struct FailingResolver;
    #[async_trait]
    impl ResolvePrice for FailingResolver {
        async fn resolve_price(
            &self,
            _params: ResolvePriceParams,
        ) -> Result<ResolvePriceResult, PaymentError> {
            Err(PaymentError::Processor("price lookup down".to_string()))
        }
    }

    fn request(method: &str, params: Option<serde_json::Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: method.to_string(),
            params,
        }
    }

    fn priced(method: &str, name: Option<&str>, amount: i64) -> PricedCapability {
        PricedCapability {
            method: method.to_string(),
            name: name.map(str::to_string),
            amount,
            max_amount: None,
            currency_unit: "sats".to_string(),
            description: None,
        }
    }

    fn one_key_meta(key: &str, value: &str) -> Meta {
        let mut m = Meta::new();
        m.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
        m
    }

    // ── capability matching ─────────────────────────────────────────

    #[test]
    fn matches_named_tool_and_wildcard_method() {
        let call = request("tools/call", Some(serde_json::json!({ "name": "echo" })));
        let entries = vec![
            priced("tools/call", Some("other"), 1),
            priced("tools/call", Some("echo"), 2),
        ];
        let hit = match_priced_capability(&call, &entries).expect("named match");
        assert_eq!(hit.amount, 2);

        // `name: None` prices every invocation of the method, first match wins.
        let wildcard = vec![
            priced("tools/call", None, 9),
            priced("tools/call", Some("echo"), 2),
        ];
        let hit = match_priced_capability(&call, &wildcard).expect("wildcard match");
        assert_eq!(hit.amount, 9);

        let miss = request("tools/list", None);
        assert!(match_priced_capability(&miss, &entries).is_none());
    }

    #[test]
    fn capability_name_reads_the_method_specific_key() {
        let call = request("tools/call", Some(serde_json::json!({ "name": "echo" })));
        assert_eq!(capability_name_for_pricing(&call).as_deref(), Some("echo"));

        let prompt = request("prompts/get", Some(serde_json::json!({ "name": "p" })));
        assert_eq!(capability_name_for_pricing(&prompt).as_deref(), Some("p"));

        let resource = request(
            "resources/read",
            Some(serde_json::json!({ "uri": "file:///x" })),
        );
        assert_eq!(
            capability_name_for_pricing(&resource).as_deref(),
            Some("file:///x")
        );

        // A non-string value and an unaddressed method both yield None.
        let numeric = request("tools/call", Some(serde_json::json!({ "name": 7 })));
        assert!(capability_name_for_pricing(&numeric).is_none());
        let other = request("tools/list", Some(serde_json::json!({ "name": "x" })));
        assert!(capability_name_for_pricing(&other).is_none());
    }

    // ── verification timeout arithmetic ─────────────────────────────

    #[test]
    fn verification_timeout_arms() {
        assert_eq!(verification_timeout(None), Duration::from_secs(300));
        assert_eq!(verification_timeout(Some(0)), Duration::from_secs(300));
        assert_eq!(verification_timeout(Some(90)), Duration::from_secs(90));
        // Multiplication overflow falls back to the default.
        assert_eq!(
            verification_timeout(Some(u64::MAX)),
            Duration::from_secs(300)
        );
    }

    // ── PMI selection (all three arms) ──────────────────────────────

    #[test]
    fn pmi_selection_prefers_the_first_client_pmi_with_a_processor() {
        let processors: Vec<Arc<dyn PaymentProcessor>> = vec![
            Arc::new(StubProcessor::new("pmi-a")),
            Arc::new(StubProcessor::new("pmi-b")),
        ];
        let by_pmi = build_processors_by_pmi(&processors);

        // First client PMI with a processor wins (skipping unknown ones).
        let client = vec!["pmi-unknown".to_string(), "pmi-b".to_string()];
        let chosen = resolve_payment_processor(Some(&client), &by_pmi, &processors).unwrap();
        assert_eq!(chosen.pmi(), "pmi-b");

        // A present-but-non-matching list falls through to the first processor.
        let unmatched = vec!["pmi-x".to_string()];
        let chosen = resolve_payment_processor(Some(&unmatched), &by_pmi, &processors).unwrap();
        assert_eq!(chosen.pmi(), "pmi-a");

        // An absent list takes the first processor.
        let chosen = resolve_payment_processor(None, &by_pmi, &processors).unwrap();
        assert_eq!(chosen.pmi(), "pmi-a");
    }

    #[test]
    fn an_empty_processor_list_is_a_configuration_error() {
        let by_pmi = HashMap::new();
        let err = match resolve_payment_processor(None, &by_pmi, &[]) {
            Err(err) => err,
            Ok(_) => panic!("an empty processor list must fail"),
        };
        assert!(err.to_string().contains("no payment processors configured"));
    }

    #[test]
    fn duplicate_pmi_registration_lets_the_last_one_win() {
        let processors: Vec<Arc<dyn PaymentProcessor>> = vec![
            Arc::new(StubProcessor {
                ttl: Some(1),
                ..StubProcessor::new("pmi-a")
            }),
            Arc::new(StubProcessor {
                ttl: Some(2),
                ..StubProcessor::new("pmi-a")
            }),
        ];
        let by_pmi = build_processors_by_pmi(&processors);
        assert_eq!(by_pmi.len(), 1);
    }

    // ── resolve-and-initiate ────────────────────────────────────────

    fn options_with(
        processors: Vec<Arc<dyn PaymentProcessor>>,
        resolver: Option<Arc<dyn ResolvePrice>>,
    ) -> ServerPaymentsOptions {
        let mut options = ServerPaymentsOptions::new(processors, Vec::new());
        if let Some(resolver) = resolver {
            options = options.with_resolve_price(resolver);
        }
        options
    }

    #[tokio::test]
    async fn static_pricing_with_no_resolver_charges_the_capability_amount() {
        let processors: Vec<Arc<dyn PaymentProcessor>> =
            vec![Arc::new(StubProcessor::new("pmi-a"))];
        let by_pmi = build_processors_by_pmi(&processors);
        let options = options_with(processors, None);
        let capability = priced("tools/call", Some("echo"), 42);
        let outcome = resolve_and_initiate_payment(
            &request("tools/call", Some(serde_json::json!({ "name": "echo" }))),
            &capability,
            "event-1",
            "client-pk",
            None,
            &options,
            &by_pmi,
        )
        .await
        .unwrap();
        match outcome {
            InitiationOutcome::PaymentRequired {
                payment_required, ..
            } => {
                assert_eq!(payment_required.amount, 42);
                assert_eq!(payment_required.pmi, "pmi-a");
            }
            _ => panic!("expected a payment request"),
        }
    }

    #[tokio::test]
    async fn a_resolver_error_is_a_server_failure_not_a_rejection() {
        let processors: Vec<Arc<dyn PaymentProcessor>> =
            vec![Arc::new(StubProcessor::new("pmi-a"))];
        let by_pmi = build_processors_by_pmi(&processors);
        let options = options_with(processors, Some(Arc::new(FailingResolver)));
        let capability = priced("tools/call", None, 1);
        let result = resolve_and_initiate_payment(
            &request("tools/call", None),
            &capability,
            "event-1",
            "client-pk",
            None,
            &options,
            &by_pmi,
        )
        .await;
        assert!(result.is_err(), "an Err quote must propagate as Err");
    }

    #[tokio::test]
    async fn a_rejection_names_the_selected_pmi_and_the_listed_amount() {
        let processors: Vec<Arc<dyn PaymentProcessor>> = vec![
            Arc::new(StubProcessor::new("pmi-a")),
            Arc::new(StubProcessor::new("pmi-b")),
        ];
        let by_pmi = build_processors_by_pmi(&processors);
        let options = options_with(
            processors,
            Some(Arc::new(StaticResolver(ResolvePriceResult::Reject {
                message: Some("too rich".to_string()),
            }))),
        );
        let capability = priced("tools/call", None, 55);
        // The client selects the non-first processor, so the rejection PMI
        // proves selection happened before pricing.
        let client = vec!["pmi-b".to_string()];
        let outcome = resolve_and_initiate_payment(
            &request("tools/call", None),
            &capability,
            "event-1",
            "client-pk",
            Some(&client),
            &options,
            &by_pmi,
        )
        .await
        .unwrap();
        match outcome {
            InitiationOutcome::Rejected {
                pmi,
                amount,
                message,
            } => {
                assert_eq!(pmi, "pmi-b");
                assert_eq!(amount, 55, "the rejection reports the listed price");
                assert_eq!(message.as_deref(), Some("too rich"));
            }
            _ => panic!("expected a rejection"),
        }
    }

    #[tokio::test]
    async fn a_waiver_initiates_nothing() {
        let processors: Vec<Arc<dyn PaymentProcessor>> =
            vec![Arc::new(StubProcessor::new("pmi-a"))];
        let by_pmi = build_processors_by_pmi(&processors);
        let options = options_with(
            processors,
            Some(Arc::new(StaticResolver(ResolvePriceResult::Waive {
                meta: Some(one_key_meta("covered_by", "promo")),
            }))),
        );
        let capability = priced("tools/call", None, 1);
        let outcome = resolve_and_initiate_payment(
            &request("tools/call", None),
            &capability,
            "event-1",
            "client-pk",
            None,
            &options,
            &by_pmi,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, InitiationOutcome::Waived));
    }

    #[tokio::test]
    async fn meta_from_the_quote_overrides_the_processor_meta() {
        let mut processor_meta = one_key_meta("source", "processor");
        processor_meta.insert(
            "kept".to_string(),
            serde_json::Value::String("yes".to_string()),
        );
        let processors: Vec<Arc<dyn PaymentProcessor>> = vec![Arc::new(StubProcessor {
            meta: Some(processor_meta),
            ..StubProcessor::new("pmi-a")
        })];
        let by_pmi = build_processors_by_pmi(&processors);
        let options = options_with(
            processors.clone(),
            Some(Arc::new(StaticResolver(ResolvePriceResult::Quote {
                amount: 5,
                description: None,
                meta: Some(one_key_meta("source", "quote")),
            }))),
        );
        let capability = priced("tools/call", None, 1);
        let outcome = resolve_and_initiate_payment(
            &request("tools/call", None),
            &capability,
            "event-1",
            "client-pk",
            None,
            &options,
            &by_pmi,
        )
        .await
        .unwrap();
        match outcome {
            InitiationOutcome::PaymentRequired { merged_meta, .. } => {
                let merged = merged_meta.expect("merged");
                assert_eq!(merged.get("source").unwrap(), "quote");
                assert_eq!(merged.get("kept").unwrap(), "yes");
            }
            _ => panic!("expected a payment request"),
        }

        // Both absent gives None, not an empty object.
        let plain: Vec<Arc<dyn PaymentProcessor>> = vec![Arc::new(StubProcessor::new("pmi-a"))];
        let by_pmi = build_processors_by_pmi(&plain);
        let options = options_with(plain, None);
        let outcome = resolve_and_initiate_payment(
            &request("tools/call", None),
            &capability,
            "event-1",
            "client-pk",
            None,
            &options,
            &by_pmi,
        )
        .await
        .unwrap();
        match outcome {
            InitiationOutcome::PaymentRequired { merged_meta, .. } => {
                assert!(merged_meta.is_none());
            }
            _ => panic!("expected a payment request"),
        }
    }

    #[tokio::test]
    async fn the_verify_timeout_derives_from_the_payment_requests_ttl() {
        let processors: Vec<Arc<dyn PaymentProcessor>> = vec![Arc::new(StubProcessor {
            ttl: Some(90),
            ..StubProcessor::new("pmi-a")
        })];
        let by_pmi = build_processors_by_pmi(&processors);
        let options = options_with(processors, None);
        let capability = priced("tools/call", None, 1);
        let outcome = resolve_and_initiate_payment(
            &request("tools/call", None),
            &capability,
            "event-1",
            "client-pk",
            None,
            &options,
            &by_pmi,
        )
        .await
        .unwrap();
        match outcome {
            InitiationOutcome::PaymentRequired { verify_timeout, .. } => {
                assert_eq!(verify_timeout, Duration::from_secs(90));
            }
            _ => panic!("expected a payment request"),
        }
    }

    #[tokio::test]
    async fn an_empty_processor_list_fails_before_pricing() {
        let by_pmi = HashMap::new();
        let options = options_with(Vec::new(), None);
        let capability = priced("tools/call", None, 1);
        let result = resolve_and_initiate_payment(
            &request("tools/call", None),
            &capability,
            "event-1",
            "client-pk",
            None,
            &options,
            &by_pmi,
        )
        .await;
        assert!(result.is_err());
    }
}
