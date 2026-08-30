//! CEP-8 payments registration for the server transport.
//!
//! [`with_server_payments`] is the one production entry point that composes the
//! payment stack onto a [`NostrServerTransport`]: it derives the announcement
//! `pmi` / `cap` / `payment_interaction` tag surface from the configuration,
//! records the negotiation policy, and registers the transparent payment
//! middleware plus, under the permissive default policy, the explicit-gating
//! middleware. The pieces it wires (the middleware factories, the tag builders,
//! the injected senders) are all public and can be hand-wired, but only this
//! entry point guarantees they agree with each other and with the wire.

use std::sync::Arc;

use crate::core::types::PaymentInteractionMode;
use crate::payments::authorization_store::AuthorizationStore;
use crate::payments::server_explicit_gating::{
    create_explicit_gating_middleware, ExplicitGatingMiddlewareParams,
};
use crate::payments::server_payments::{
    create_server_payments_middleware, ServerPaymentsMiddlewareParams, ServerPaymentsOptions,
};
use crate::payments::server_payments_utils::build_processors_by_pmi;
use crate::payments::tags::{cap_tags_from_priced_capabilities, payment_interaction_tag, pmi_tag};
use crate::payments::types::PaymentInteractionPolicy;
use crate::transport::server::NostrServerTransport;
use nostr_sdk::prelude::Tag;

const LOG_TARGET: &str = "contextvm_sdk::payments::server_transport_payments";

/// Attach CEP-8 payments to a [`NostrServerTransport`].
///
/// This is the production registration path for server-side payments. In order, it:
/// builds the PMI-to-processor map once and shares it across both lifecycles; sets the
/// announcement extra tags (one `pmi` tag per processor in registration order, plus a
/// single `payment_interaction=explicit_gating` availability tag when the policy is
/// [`PaymentInteractionPolicy::Optional`]); sets the announcement pricing tags (one
/// `cap` tag per advertisable priced capability); records the payment-interaction
/// policy for session negotiation; and registers the transparent payment middleware,
/// followed (under `Optional`) by the explicit-gating middleware with a fresh
/// [`AuthorizationStore`]. A `Transparent` policy advertises no `payment_interaction`
/// tag and registers no gating middleware; an `explicit_gating` session request is then
/// rejected with a JSON-RPC `-32602`.
///
/// # Registration contract
///
/// Call this exactly once, after constructing the transport and before
/// [`start`](NostrServerTransport::start). Both misuses are refused with an error
/// before any state changes: on a started transport the middleware chain and policy
/// are already frozen, so registration would silently take no effect while the live
/// tag setters still advertise payments (priced requests would execute for free); and
/// a second registration would append a second middleware pair that charges every
/// priced request twice. The transport is not restartable after
/// [`close`](NostrServerTransport::close), so a post-close registration is inert.
/// Register before [`announce`](NostrServerTransport::announce), or the first kind
/// 11316 announcement ships without payment tags.
///
/// # Tag ownership
///
/// This function owns the announcement extra-tag slot: it replaces any extra tags set
/// earlier through
/// [`set_announcement_extra_tags`](NostrServerTransport::set_announcement_extra_tags),
/// exactly as the reference implementation does. Calling either tag setter after
/// registration is also unsupported: announcements and the normal response path read
/// the new set live, but the payment senders registered here capture the tag sets at
/// registration time and keep emitting the originals, so payment events diverge from
/// responses (and the replacement wipes the `pmi` and availability tags from the live
/// paths). Let this function own both tag slots.
///
/// Pricing advertisement has one deliberate asymmetry, shared with the reference
/// implementation: a priced capability whose `name` is `None` or whose `method` is not
/// one of `tools/call` / `prompts/get` / `resources/read` still prices matching
/// requests but produces no `cap` tag, so it is billable without being advertised.
///
/// # Configuration warnings
///
/// Configuration is not validated, matching the reference implementation: empty
/// processor or priced-capability lists register anyway and surface failures at
/// request time. Two configurations log a warning here instead of failing. A
/// `payment_ttl` above the transport's session timeout warns because the paying
/// client's session can expire before the payment resolves, which costs the client the
/// acceptance notification even though the paid result still delivers from the
/// captured route snapshot. Priced capabilities with no processors warn because every
/// priced request will fail processor selection at request time and be dropped.
///
/// # Rate limiting
///
/// Every successful payment offer, in either lifecycle, spawns one detached
/// verification task, unbounded across identities and unauthenticated peers.
/// `max_pending_payments` and the authorization store's entry caps bound state, not
/// tasks. CEP-8 explicitly leaves rate limiting and abuse prevention to
/// implementations and sanctions only discretionary eviction, so deployments that
/// cannot trust their peer set should bound intake upstream: `allowed_public_keys`,
/// relay-side policy, or an external limiter.
///
/// # State lifetime
///
/// All payment state is in-memory and single-process: the pending-payment dedup, the
/// authorization store's pending and granted entries, and the payment route snapshots
/// are all forgotten on restart, and an LRU-evicted authorization re-invoices its
/// payer on retry.
///
/// # Errors
///
/// Fails without mutating the transport when the transport is already started, or
/// when a payment-interaction policy is already recorded (a prior registration, or a
/// hand-set policy via
/// [`set_supported_payment_interaction`](NostrServerTransport::set_supported_payment_interaction);
/// this function registers once and owns the policy).
pub fn with_server_payments(
    transport: &mut NostrServerTransport,
    options: ServerPaymentsOptions,
) -> crate::Result<()> {
    // Both guards run before any mutation, so a failed call leaves the transport
    // exactly as it was and the caller can correct and retry.
    if transport.is_started() {
        return Err(crate::Error::Other(
            "with_server_payments must be called before start()".to_string(),
        ));
    }
    if transport.supported_payment_interaction().is_some() {
        return Err(crate::Error::Other(
            "a payment interaction policy is already recorded on this transport; \
             with_server_payments registers once and owns the policy"
                .to_string(),
        ));
    }

    // Build the PMI-to-processor map once and share it across both middlewares, so
    // the duplicate-PMI warning fires once per duplicate occurrence in total.
    let shared_processors = Arc::new(build_processors_by_pmi(&options.processors));

    // Log-only configuration warnings; neither changes behavior or the wire.
    let session_timeout = transport.session_timeout();
    if options.payment_ttl > session_timeout {
        tracing::warn!(
            target: LOG_TARGET,
            payment_ttl = ?options.payment_ttl,
            session_timeout = ?session_timeout,
            "payment_ttl exceeds the transport's session timeout: a paying client's \
             session can expire before its payment resolves, costing the client the \
             acceptance notification (the paid result itself still delivers from the \
             captured route snapshot)"
        );
    }
    if options.processors.is_empty() && !options.priced_capabilities.is_empty() {
        tracing::warn!(
            target: LOG_TARGET,
            priced_capabilities = options.priced_capabilities.len(),
            "priced capabilities are configured but no payment processors are: every \
             priced request will fail processor selection at request time and be dropped"
        );
    }

    let policy = options.payment_interaction;
    let supports_explicit_gating = policy == PaymentInteractionPolicy::Optional;

    transport.set_announcement_extra_tags(compose_payment_extra_tags(
        &options,
        supports_explicit_gating,
    ));
    transport.set_announcement_pricing_tags(cap_tags_from_priced_capabilities(
        &options.priced_capabilities,
    ));
    transport.set_supported_payment_interaction(policy);

    // Both senders capture the announcement tag sets at the moment they are built, so
    // they are built only after both tag setters above have run. A sender built
    // earlier would ship an empty discovery replay on every payment event for the
    // life of the process.
    let notification_sender = transport.payment_notification_sender(options.payment_ttl);
    transport.add_inbound_middleware(create_server_payments_middleware(
        ServerPaymentsMiddlewareParams {
            options: options.clone(),
            sender: notification_sender,
            processors_by_pmi: Some(Arc::clone(&shared_processors)),
        },
    ));

    // The transparent middleware self-gates on the per-session effective mode, so it
    // is safe to register the explicit-gating middleware alongside it: each request is
    // routed to exactly one lifecycle based on the negotiated mode.
    if supports_explicit_gating {
        let targeted_sender = transport.targeted_response_sender();
        transport.add_inbound_middleware(create_explicit_gating_middleware(
            ExplicitGatingMiddlewareParams {
                options,
                sender: targeted_sender,
                authorization_store: AuthorizationStore::new(),
                processors_by_pmi: Some(shared_processors),
            },
        ));
    }

    Ok(())
}

/// The announcement extra-tag segment: one `pmi` tag per processor in registration
/// order (duplicates preserved, mirroring the reference implementation's wire), and,
/// when explicit gating is supported (the [`PaymentInteractionPolicy::Optional`]
/// policy), a single `payment_interaction=explicit_gating` availability tag pushed
/// last. A `Transparent` policy advertises no `payment_interaction` tag at all. The
/// caller passes the same `supports_explicit_gating` bool that drives the policy
/// recording and the conditional gating registration, so the three consumers cannot
/// drift apart.
fn compose_payment_extra_tags(
    options: &ServerPaymentsOptions,
    supports_explicit_gating: bool,
) -> Vec<Tag> {
    let mut tags: Vec<Tag> = options
        .processors
        .iter()
        .map(|processor| pmi_tag(processor.pmi()))
        .collect();
    if supports_explicit_gating {
        tags.push(payment_interaction_tag(
            PaymentInteractionMode::ExplicitGating,
        ));
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::constants::SERVER_ANNOUNCEMENT_KIND;
    use crate::core::types::ServerInfo;
    use crate::payments::errors::PaymentError;
    use crate::payments::traits::PaymentProcessor;
    use crate::payments::types::{
        PaymentProcessorCreateParams, PaymentProcessorVerifyParams, PaymentRequiredParams,
        PricedCapability, VerifyOutcome,
    };
    use crate::relay::mock::MockRelayPool;
    use crate::relay::RelayPoolTrait;
    use crate::transport::server::NostrServerTransportConfig;
    use async_trait::async_trait;
    use std::io::Write;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tracing_subscriber::fmt::MakeWriter;

    /// A minimal local processor double, so these tests run in every feature
    /// configuration (the deterministic fakes are `test-utils`-gated).
    struct StubProcessor {
        pmi: String,
    }

    impl StubProcessor {
        fn arc(pmi: &str) -> Arc<dyn PaymentProcessor> {
            Arc::new(Self {
                pmi: pmi.to_string(),
            })
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
            Ok(PaymentRequiredParams {
                amount: params.amount,
                pay_req: format!("invoice-{}", params.request_event_id),
                pmi: self.pmi.clone(),
                description: params.description,
                ttl: None,
                meta: None,
            })
        }

        async fn verify_payment(
            &self,
            _params: PaymentProcessorVerifyParams,
        ) -> Result<VerifyOutcome, PaymentError> {
            Ok(VerifyOutcome::default())
        }
    }

    fn priced_tool(name: &str, amount: i64) -> PricedCapability {
        PricedCapability {
            method: "tools/call".to_string(),
            name: Some(name.to_string()),
            amount,
            max_amount: None,
            currency_unit: "sats".to_string(),
            description: None,
        }
    }

    fn options(
        pmis: &[&str],
        priced: Vec<PricedCapability>,
        policy: PaymentInteractionPolicy,
    ) -> ServerPaymentsOptions {
        ServerPaymentsOptions::new(pmis.iter().map(|p| StubProcessor::arc(p)).collect(), priced)
            .with_payment_interaction(policy)
    }

    async fn transport() -> NostrServerTransport {
        transport_with(NostrServerTransportConfig::default()).await
    }

    async fn transport_with(config: NostrServerTransportConfig) -> NostrServerTransport {
        NostrServerTransport::with_relay_pool(
            config,
            Arc::new(MockRelayPool::new()) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport")
    }

    fn tag_tuples(tags: &[Tag]) -> Vec<Vec<String>> {
        tags.iter().map(|t| t.clone().to_vec()).collect()
    }

    /// A thread-local tracing capture, so warn-count asserts do not race parallel tests.
    #[derive(Clone, Default)]
    struct Capture(Arc<StdMutex<Vec<u8>>>);

    impl Capture {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    fn warn_capture() -> (Capture, tracing::subscriber::DefaultGuard) {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(capture.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (capture, guard)
    }

    // ── tag composition ─────────────────────────────────────────────

    /// Compose with the same policy-to-bool binding the entry point uses.
    fn compose(options: &ServerPaymentsOptions) -> Vec<Tag> {
        compose_payment_extra_tags(
            options,
            options.payment_interaction == PaymentInteractionPolicy::Optional,
        )
    }

    /// The composed extra segment, asserted as an ordered sequence per policy: the
    /// availability tag exists only under `Optional` and is pushed last; duplicates
    /// and registration order are preserved; empty processors yield no `pmi` tags.
    #[test]
    fn composition_per_policy() {
        let optional = options(
            &["pmi:A", "pmi:B"],
            vec![],
            PaymentInteractionPolicy::Optional,
        );
        assert_eq!(
            tag_tuples(&compose(&optional)),
            vec![
                vec!["pmi".to_string(), "pmi:A".to_string()],
                vec!["pmi".to_string(), "pmi:B".to_string()],
                vec![
                    "payment_interaction".to_string(),
                    "explicit_gating".to_string()
                ],
            ],
            "the availability tag must be present exactly once, ordered last"
        );

        let transparent = options(
            &["pmi:A", "pmi:B"],
            vec![],
            PaymentInteractionPolicy::Transparent,
        );
        assert_eq!(
            tag_tuples(&compose(&transparent)),
            vec![
                vec!["pmi".to_string(), "pmi:A".to_string()],
                vec!["pmi".to_string(), "pmi:B".to_string()],
            ],
            "a transparent-only policy advertises no payment_interaction tag"
        );

        let duplicated = options(
            &["pmi:A", "pmi:A"],
            vec![],
            PaymentInteractionPolicy::Transparent,
        );
        assert_eq!(
            tag_tuples(&compose(&duplicated)),
            vec![
                vec!["pmi".to_string(), "pmi:A".to_string()],
                vec!["pmi".to_string(), "pmi:A".to_string()],
            ],
            "duplicate processors emit duplicate pmi tags (reference parity)"
        );

        let empty = options(&[], vec![], PaymentInteractionPolicy::Optional);
        assert_eq!(
            tag_tuples(&compose(&empty)),
            vec![vec![
                "payment_interaction".to_string(),
                "explicit_gating".to_string()
            ]],
            "no processors yield no pmi tags"
        );
    }

    // ── guards ──────────────────────────────────────────────────────

    /// A post-start registration is refused with a clean error before any mutation:
    /// the policy stays unrecorded and the announcement carries no payment tags.
    #[tokio::test]
    async fn post_start_call_errors_without_mutating() {
        let pool = Arc::new(MockRelayPool::new());
        let mut server = NostrServerTransport::with_relay_pool(
            NostrServerTransportConfig::default().with_server_info(ServerInfo {
                name: Some("guard-test".to_string()),
                ..Default::default()
            }),
            Arc::clone(&pool) as Arc<dyn RelayPoolTrait>,
        )
        .await
        .expect("server transport");
        server.start().await.expect("start");

        let result = with_server_payments(
            &mut server,
            options(
                &["pmi:A"],
                vec![priced_tool("paid-tool", 21)],
                PaymentInteractionPolicy::Optional,
            ),
        );
        let error = result.expect_err("a post-start registration must be refused");
        assert!(
            error.to_string().contains("before start()"),
            "unexpected error: {error}"
        );

        assert_eq!(
            server.supported_payment_interaction(),
            None,
            "the refused call must not record a policy"
        );
        server.announce().await.expect("announce");
        let announcement = pool
            .stored_events()
            .await
            .into_iter()
            .find(|e| e.kind.as_u16() == SERVER_ANNOUNCEMENT_KIND)
            .expect("announcement published");
        let announcement_tags: Vec<Tag> = announcement.tags.iter().cloned().collect();
        for tag in tag_tuples(&announcement_tags) {
            assert!(
                !matches!(
                    tag.first().map(String::as_str),
                    Some("pmi") | Some("cap") | Some("payment_interaction")
                ),
                "the refused call must not set announcement tags, found {tag:?}"
            );
        }
        server.close().await.expect("close");
    }

    /// A second registration is refused and the first registration's policy stands.
    #[tokio::test]
    async fn second_call_errors_and_first_registration_stands() {
        let mut server = transport().await;
        with_server_payments(
            &mut server,
            options(
                &["pmi:A"],
                vec![priced_tool("paid-tool", 21)],
                PaymentInteractionPolicy::Optional,
            ),
        )
        .expect("the first registration succeeds");

        let error = with_server_payments(
            &mut server,
            options(
                &["pmi:B"],
                vec![priced_tool("other-tool", 5)],
                PaymentInteractionPolicy::Transparent,
            ),
        )
        .expect_err("a second registration must be refused");
        assert!(
            error
                .to_string()
                .contains("a payment interaction policy is already recorded"),
            "unexpected error: {error}"
        );

        assert_eq!(
            server.supported_payment_interaction(),
            Some(PaymentInteractionPolicy::Optional),
            "the refused second call must not overwrite the first policy"
        );
    }

    // ── warns ───────────────────────────────────────────────────────

    /// The duplicate-PMI warning fires once per duplicate occurrence in total, because
    /// the processor map is built once and shared across both registered middlewares.
    #[tokio::test]
    async fn duplicate_pmi_warns_once_across_both_middlewares() {
        let (capture, _guard) = warn_capture();
        let mut server = transport().await;
        with_server_payments(
            &mut server,
            options(
                &["pmi:A", "pmi:A"],
                vec![priced_tool("paid-tool", 21)],
                PaymentInteractionPolicy::Optional,
            ),
        )
        .expect("registers");

        let logs = capture.contents();
        assert_eq!(
            logs.matches("duplicate PMI processor registered").count(),
            1,
            "the shared map must be built exactly once, logs:\n{logs}"
        );
        assert!(
            !logs.contains("no payment processors"),
            "no other configuration warning applies here, logs:\n{logs}"
        );
    }

    /// The TTL warning fires exactly when `payment_ttl` strictly exceeds the session
    /// timeout, naming both durations; the equal-defaults configuration stays silent.
    #[tokio::test]
    async fn ttl_warn_fires_only_above_session_timeout() {
        let above = {
            let (capture, _guard) = warn_capture();
            let mut server = transport_with(
                NostrServerTransportConfig::default()
                    .with_session_timeout(Duration::from_secs(300)),
            )
            .await;
            with_server_payments(
                &mut server,
                options(
                    &["pmi:A"],
                    vec![priced_tool("paid-tool", 21)],
                    PaymentInteractionPolicy::Optional,
                )
                .with_payment_ttl(Duration::from_secs(301)),
            )
            .expect("registers");
            capture.contents()
        };
        assert_eq!(
            above
                .matches("exceeds the transport's session timeout")
                .count(),
            1,
            "one warning per registration, logs:\n{above}"
        );
        assert!(
            above.contains("301") && above.contains("300"),
            "the warning must name both durations, logs:\n{above}"
        );

        let equal = {
            let (capture, _guard) = warn_capture();
            let mut server = transport_with(
                NostrServerTransportConfig::default()
                    .with_session_timeout(Duration::from_secs(300)),
            )
            .await;
            with_server_payments(
                &mut server,
                options(
                    &["pmi:A"],
                    vec![priced_tool("paid-tool", 21)],
                    PaymentInteractionPolicy::Optional,
                )
                .with_payment_ttl(Duration::from_secs(300)),
            )
            .expect("registers");
            capture.contents()
        };
        assert!(
            !equal.contains("exceeds the transport's session timeout"),
            "an equal (default) configuration must stay silent, logs:\n{equal}"
        );
    }

    /// Priced capabilities with no processors warn loudly at registration time but
    /// still register, preserving the reference implementation's no-validation posture.
    #[tokio::test]
    async fn empty_processors_with_priced_caps_warns_and_still_registers() {
        let (capture, _guard) = warn_capture();
        let mut server = transport().await;
        let result = with_server_payments(
            &mut server,
            options(
                &[],
                vec![priced_tool("paid-tool", 21)],
                PaymentInteractionPolicy::Optional,
            ),
        );
        assert!(result.is_ok(), "registration must not validate");

        let logs = capture.contents();
        assert_eq!(
            logs.matches("no payment processors are").count(),
            1,
            "the misconfiguration must be loud at registration time, logs:\n{logs}"
        );
        assert_eq!(
            server.supported_payment_interaction(),
            Some(PaymentInteractionPolicy::Optional),
            "the policy is recorded despite the warning"
        );
    }
}
