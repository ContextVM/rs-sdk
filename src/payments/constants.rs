//! CEP-8 payment protocol constants (methods, JSON-RPC error codes, PMIs, TTLs).

/// `notifications/payment_required`: server requests payment (transparent lifecycle).
pub const PAYMENT_REQUIRED_METHOD: &str = "notifications/payment_required";
/// `notifications/payment_accepted`: server observed settlement.
pub const PAYMENT_ACCEPTED_METHOD: &str = "notifications/payment_accepted";
/// `notifications/payment_rejected`: server refused or rejected payment.
pub const PAYMENT_REJECTED_METHOD: &str = "notifications/payment_rejected";

/// Explicit-gating JSON-RPC error: payment required (`error.data` = [`PaymentRequiredErrorData`](crate::payments::PaymentRequiredErrorData)).
pub const PAYMENT_REQUIRED_ERROR_CODE: i64 = -32042;
/// Explicit-gating JSON-RPC error: payment pending (`error.data` = [`PaymentPendingErrorData`](crate::payments::PaymentPendingErrorData)).
pub const PAYMENT_PENDING_ERROR_CODE: i64 = -32043;
/// Unsupported `payment_interaction` negotiation: reuses JSON-RPC Invalid Params per CEP-8.
pub const UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE: i64 = -32602;

/// The only Phase-A payment method identifier.
pub const PMI_BITCOIN_LIGHTNING_BOLT11: &str = "bitcoin-lightning-bolt11";

/// Default payment TTL when `payment_required` carries no `ttl` (ms). Mirrors the server default.
pub const DEFAULT_PAYMENT_TTL_MS: u64 = 300_000;
/// Default synthetic-progress heartbeat interval (ms). Half the 60 s MCP request timeout.
pub const DEFAULT_SYNTHETIC_PROGRESS_INTERVAL_MS: u64 = 30_000;
/// Per-map cap for the `AuthorizationStore` LRUs. Equals [`core::constants::DEFAULT_LRU_SIZE`](crate::core::constants::DEFAULT_LRU_SIZE).
pub const AUTH_STORE_MAX_ENTRIES: usize = 5000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_match_ts_sdk() {
        assert_eq!(PAYMENT_REQUIRED_METHOD, "notifications/payment_required");
        assert_eq!(PAYMENT_ACCEPTED_METHOD, "notifications/payment_accepted");
        assert_eq!(PAYMENT_REJECTED_METHOD, "notifications/payment_rejected");
    }

    #[test]
    fn error_codes_match_ts_sdk() {
        assert_eq!(PAYMENT_REQUIRED_ERROR_CODE, -32042);
        assert_eq!(PAYMENT_PENDING_ERROR_CODE, -32043);
        assert_eq!(UNSUPPORTED_PAYMENT_INTERACTION_ERROR_CODE, -32602);
    }

    #[test]
    fn pmi_and_ttls_match_ts_sdk() {
        assert_eq!(PMI_BITCOIN_LIGHTNING_BOLT11, "bitcoin-lightning-bolt11");
        assert_eq!(DEFAULT_PAYMENT_TTL_MS, 300_000);
        assert_eq!(DEFAULT_SYNTHETIC_PROGRESS_INTERVAL_MS, 30_000);
        assert_eq!(AUTH_STORE_MAX_ENTRIES, 5000);
        // Mirrors the core LRU default; keep the two in lockstep.
        assert_eq!(
            AUTH_STORE_MAX_ENTRIES,
            crate::core::constants::DEFAULT_LRU_SIZE
        );
    }
}
