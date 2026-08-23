//! CEP-8 explicit-gating authorization store: a bounded, TTL-aware, in-memory store
//! of pending and granted payment authorizations, keyed by canonical invocation
//! identity (`"{client_pubkey}:{invocation_hash}"`).
//!
//! It records the two states the explicit-gating middleware transitions between:
//! `pending` (a payment is in flight) and granted (paid, ready to consume exactly
//! once). Each state is a bounded LRU (default cap [`AUTH_STORE_MAX_ENTRIES`]) that
//! evicts the least-recently-used entry under memory pressure; expiry is lazy
//! (checked on access, no background sweeper). This is the Rust port of the ts-sdk
//! `AuthorizationStore`.
//!
//! ## Concurrency
//!
//! Every operation is a single `std::sync::Mutex<Inner>` critical section with no
//! `.await` inside, so a check-and-mutate (`claim`, `try_set_pending`) is atomic even
//! across a multi-threaded tokio runtime. This gives rs-sdk the double-spend safety
//! that ts-sdk gets for free from Node's single-threaded event loop: at most one
//! concurrent `claim` consumes a grant, and at most one concurrent `try_set_pending`
//! transitions an identity to pending; every other caller observes the mutation and
//! backs off.
//!
//! ## Single-process only
//!
//! The atomicity above is in-process. Multi-process horizontal scaling needs an
//! external distributed lock (e.g. Redis Redlock) keyed by the canonical invocation
//! identity to prevent duplicate payments, mirroring the ts-sdk reference.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lru::LruCache;

use crate::payments::canonical::CanonicalInvocationIdentity;
use crate::payments::constants::AUTH_STORE_MAX_ENTRIES;

/// State behind the single mutex.
///
/// Two independent LRUs so grants and pending payments evict separately (the ts-sdk
/// uses two `LruCache` instances with the same cap). A merged, shared-capacity map
/// could evict a paid grant on a concurrent pending write, so keep them distinct.
struct Inner {
    /// Granted authorizations: composite key -> grant expiry. Single-use (consumed on `claim`).
    authorizations: LruCache<String, Instant>,
    /// In-flight payments: composite key -> pending expiry.
    pending: LruCache<String, Instant>,
}

/// The outcome of [`AuthorizationStore::claim_or_set_pending`], the explicit-gating
/// middleware's entry sequence.
///
/// `#[non_exhaustive]`: CEP-8 leaves room for policies granting a different number of
/// executions per payment, so a future variant must not be a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOrPending {
    /// A live grant was consumed (single-use): the caller forwards the invocation.
    Claimed,
    /// No grant and no live pending existed; this call transitioned the identity to
    /// pending and the caller now owns the offer cycle (emits `-32042`).
    PendingSet,
    /// A payment is already pending and still active: the caller emits `-32043`.
    AlreadyPending {
        /// Milliseconds until the live pending entry expires, measured at the same
        /// instant the pending check ran (so it cannot race to a stale read).
        remaining_ms: u64,
    },
}

/// A bounded, TTL-aware store for explicit-gating authorizations (pending + granted).
///
/// Cheap to clone (`Arc`-backed) and `Send + Sync`: the explicit-gating middleware and
/// its detached `verify_payment` task share one store via clones. All methods take
/// `&self`; the per-call critical section holds a `std::sync::Mutex` with no `.await`
/// inside, giving the check-and-mutate atomicity that ts-sdk gets from the
/// single-threaded event loop.
#[derive(Clone)]
pub struct AuthorizationStore {
    inner: Arc<Mutex<Inner>>,
}

impl Default for AuthorizationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthorizationStore {
    /// Create a store with the default per-map cap ([`AUTH_STORE_MAX_ENTRIES`], 5000).
    pub fn new() -> Self {
        Self::with_max_entries(AUTH_STORE_MAX_ENTRIES)
    }

    /// Create a store with a per-map LRU cap. A cap of `0` is clamped to `1` (repo idiom),
    /// so the store is always usable.
    pub fn with_max_entries(max_entries: usize) -> Self {
        let cap = NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::new(1).unwrap());
        Self {
            inner: Arc::new(Mutex::new(Inner {
                authorizations: LruCache::new(cap),
                pending: LruCache::new(cap),
            })),
        }
    }

    /// Lock-poison-tolerant access to the inner state (the repo's poison-recovery idiom).
    /// The critical sections are panic-free by construction (`now`/`expires_at` are computed
    /// before the lock; only string hashing, `Instant` copies, integer compares, and
    /// infallible LRU ops run under it), so poisoning should never occur; recover via
    /// `into_inner()` regardless, never `.unwrap()`.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The composite LRU key, `"{client_pubkey}:{invocation_hash}"`. Both halves are
    /// fixed-width hex, so the `:` delimiter is unambiguous and no two distinct identities
    /// can collide.
    fn key(id: &CanonicalInvocationIdentity) -> String {
        format!("{}:{}", id.client_pubkey, id.invocation_hash)
    }

    /// Record a paid authorization valid for `ttl_ms`, and clear any pending state for the
    /// same identity (once granted it is no longer pending). Each grant authorizes exactly
    /// one future execution; a re-grant overwrites in place.
    pub fn grant(&self, id: &CanonicalInvocationIdentity, ttl_ms: u64) {
        let expires_at = Instant::now() + Duration::from_millis(ttl_ms);
        let key = Self::key(id);

        let mut inner = self.lock();
        // Pop pending first so the owned key can move into `put` with no clone; order is
        // immaterial under the lock (no observer sees an intermediate state).
        inner.pending.pop(&key);
        inner.authorizations.put(key, expires_at);
    }

    /// Atomically consume the single-use authorization for `id`.
    ///
    /// Returns `true` if a live grant was claimed (and removes it), `false` if none exists
    /// or it has expired. The removal is unconditional and inside the one critical section,
    /// so two concurrent claims can never both succeed: this is the double-spend linchpin.
    pub fn claim(&self, id: &CanonicalInvocationIdentity) -> bool {
        let now = Instant::now();
        let key = Self::key(id);

        let mut inner = self.lock();
        // `pop` collapses the ts get-then-delete: every found path deletes, so removing
        // unconditionally and judging expiry on the returned value is observably identical
        // and atomically single-use.
        match inner.authorizations.pop(&key) {
            None => false,
            Some(expires_at) => now <= expires_at,
        }
    }

    /// Atomic check-and-set of pending state for `id`.
    ///
    /// Returns `true` if this call transitioned the identity to pending (the caller emits
    /// `-32042` Payment Required); `false` if a payment is already pending and still active
    /// (the caller emits `-32043` Payment Pending). An expired pending entry is overwritten.
    /// This is what prevents concurrent duplicates from both starting a payment flow.
    pub fn try_set_pending(&self, id: &CanonicalInvocationIdentity, ttl_ms: u64) -> bool {
        let now = Instant::now();
        let expires_at = now + Duration::from_millis(ttl_ms);
        let key = Self::key(id);

        let mut inner = self.lock();
        // `get` (not `peek`) promotes to MRU, matching the ts read semantics. `.copied()`
        // ends the borrow so the later `put` type-checks.
        if let Some(existing) = inner.pending.get(&key).copied() {
            if now <= existing {
                // Already pending and active.
                return false;
            }
            // Expired: fall through and overwrite.
        }
        inner.pending.put(key, expires_at);
        true
    }

    /// Tighten or extend the TTL of an already-pending authorization to `ttl_ms` from now.
    ///
    /// No-op if there is no pending entry for `id`, or the existing one has already expired
    /// (the stale entry is left in place, as in ts-sdk).
    pub fn update_pending_ttl(&self, id: &CanonicalInvocationIdentity, ttl_ms: u64) {
        let now = Instant::now();
        let expires_at = now + Duration::from_millis(ttl_ms);
        let key = Self::key(id);

        let mut inner = self.lock();
        if let Some(existing) = inner.pending.get(&key).copied() {
            // `<=` treats the exact endpoint as still active (the complement of `claim`'s
            // `now > expiry` = expired).
            if now <= existing {
                inner.pending.put(key, expires_at);
            }
        }
    }

    /// The remaining TTL in milliseconds for a pending authorization, or `0` if there is
    /// none or it has expired. Used to compute the client's `retry_after` hint.
    ///
    /// Note this is not a side-effect-free read: like ts-sdk it promotes the entry to MRU
    /// (`lru::get`), so it influences eviction order.
    pub fn get_pending_remaining_ms(&self, id: &CanonicalInvocationIdentity) -> u64 {
        let now = Instant::now();
        let key = Self::key(id);

        let mut inner = self.lock();
        match inner.pending.get(&key).copied() {
            // `saturating_duration_since` is `0` once `now >= expires_at`, matching ts-sdk's
            // `remaining > 0 ? remaining : 0`.
            Some(expires_at) => expires_at.saturating_duration_since(now).as_millis() as u64,
            None => 0,
        }
    }

    /// Clear any pending state for `id` (e.g. on verification failure or expiry).
    pub fn clear_pending(&self, id: &CanonicalInvocationIdentity) {
        let key = Self::key(id);
        self.lock().pending.pop(&key);
    }

    /// Atomically claim a live grant for `id`, or else transition it to pending: the
    /// explicit-gating middleware's entry sequence, composed into one critical section.
    ///
    /// In order, under one lock: a live grant is consumed ([`ClaimOrPending::Claimed`];
    /// any pending entry is left intact, since it belongs to another offer cycle); an
    /// expired grant is dropped; a live pending entry parks the caller
    /// ([`ClaimOrPending::AlreadyPending`] with the remaining TTL computed from the same
    /// instant); otherwise pending is set for `ttl_ms` ([`ClaimOrPending::PendingSet`]).
    ///
    /// The composition exists because the two-call sequence (`claim` then
    /// `try_set_pending`) is atomic in ts-sdk only through JavaScript run-to-completion.
    /// Here the detached verify task grants from another thread, and a grant landing
    /// between the two calls would mint a spurious second invoice while a paid grant
    /// sits unclaimed. One critical section removes that interleaving entirely.
    pub fn claim_or_set_pending(
        &self,
        id: &CanonicalInvocationIdentity,
        ttl_ms: u64,
    ) -> ClaimOrPending {
        let now = Instant::now();
        let expires_at = now + Duration::from_millis(ttl_ms);
        let key = Self::key(id);

        let mut inner = self.lock();
        // Grant first (the ts order): a found grant is popped unconditionally and judged
        // on the returned expiry, exactly like `claim`. An expired grant falls through.
        if let Some(grant_expires) = inner.authorizations.pop(&key) {
            if now <= grant_expires {
                return ClaimOrPending::Claimed;
            }
        }
        // Then pending, exactly like `try_set_pending`, except the remaining TTL is
        // computed from the same `now` inside the same critical section.
        if let Some(existing) = inner.pending.get(&key).copied() {
            if now <= existing {
                let remaining_ms = existing.saturating_duration_since(now).as_millis() as u64;
                return ClaimOrPending::AlreadyPending { remaining_ms };
            }
        }
        inner.pending.put(key, expires_at);
        ClaimOrPending::PendingSet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::Barrier;
    use tokio::task::JoinSet;

    fn id(client_pubkey: &str, invocation_hash: &str) -> CanonicalInvocationIdentity {
        CanonicalInvocationIdentity {
            client_pubkey: client_pubkey.into(),
            invocation_hash: invocation_hash.into(),
        }
    }

    fn default_id() -> CanonicalInvocationIdentity {
        id("client-1", "hash-1")
    }

    // ── single-thread invariants (1:1 with the ts-sdk authorization-store test suite) ──

    #[test]
    fn grant_then_claim_is_single_use() {
        let store = AuthorizationStore::new();
        let id = default_id();

        assert!(!store.claim(&id)); // nothing to claim before a grant
        store.grant(&id, 10_000);
        assert!(store.claim(&id)); // first claim consumes the grant
        assert!(!store.claim(&id)); // single-use: second claim finds nothing
    }

    #[test]
    fn claim_fails_after_grant_ttl() {
        let store = AuthorizationStore::new();
        let id = default_id();

        store.grant(&id, 50);
        // Real sleep: the store uses std::time::Instant, invisible to tokio's virtual clock.
        std::thread::sleep(Duration::from_millis(150));
        assert!(!store.claim(&id));
    }

    #[test]
    fn try_set_pending_blocks_a_concurrent_duplicate() {
        let store = AuthorizationStore::new();
        let id = default_id();

        assert!(store.try_set_pending(&id, 10_000)); // first transitions to pending
        assert!(!store.try_set_pending(&id, 10_000)); // second is blocked
        assert!(store.get_pending_remaining_ms(&id) > 0); // pending is observable
    }

    #[test]
    fn re_pending_after_clear() {
        let store = AuthorizationStore::new();
        let id = default_id();

        assert!(store.try_set_pending(&id, 10_000));
        assert!(!store.try_set_pending(&id, 10_000));
        store.clear_pending(&id);
        assert!(store.try_set_pending(&id, 10_000)); // clear released the slot
    }

    #[test]
    fn re_pending_after_pending_ttl_expiry() {
        let store = AuthorizationStore::new();
        let id = default_id();

        assert!(store.try_set_pending(&id, 50));
        assert!(!store.try_set_pending(&id, 50));
        std::thread::sleep(Duration::from_millis(150));
        assert!(store.try_set_pending(&id, 50)); // expired pending can be re-set
    }

    #[test]
    fn grant_clears_pending() {
        let store = AuthorizationStore::new();
        let id = default_id();

        assert!(store.try_set_pending(&id, 10_000));
        store.grant(&id, 10_000);
        // grant cleared pending, so a fresh try_set_pending succeeds again
        assert!(store.try_set_pending(&id, 10_000));
    }

    #[test]
    fn grant_lru_eviction_at_capacity() {
        let store = AuthorizationStore::with_max_entries(2);
        let id1 = id("client", "h1");
        let id2 = id("client", "h2");
        let id3 = id("client", "h3");

        store.grant(&id1, 10_000);
        store.grant(&id2, 10_000);
        store.grant(&id3, 10_000); // evicts the LRU grant (id1)

        assert!(!store.claim(&id1)); // evicted
        assert!(store.claim(&id2));
        assert!(store.claim(&id3));
    }

    #[test]
    fn pending_lru_eviction_at_capacity() {
        let store = AuthorizationStore::with_max_entries(2);
        let id1 = id("client", "p1");
        let id2 = id("client", "p2");
        let id3 = id("client", "p3");

        store.try_set_pending(&id1, 10_000);
        store.try_set_pending(&id2, 10_000);
        store.try_set_pending(&id3, 10_000); // evicts the LRU pending (id1)

        assert_eq!(store.get_pending_remaining_ms(&id1), 0); // evicted
        assert!(store.get_pending_remaining_ms(&id2) > 0);
        assert!(store.get_pending_remaining_ms(&id3) > 0);
    }

    #[test]
    fn update_pending_ttl_and_get_remaining() {
        let store = AuthorizationStore::new();
        let id = default_id();

        // (1) remaining right after try_set_pending is in (0, 100]
        assert!(store.try_set_pending(&id, 100));
        let after_set = store.get_pending_remaining_ms(&id);
        assert!(after_set > 0);
        assert!(after_set <= 100);

        // (2) update_pending_ttl extends the live pending TTL into (100, 500]
        store.update_pending_ttl(&id, 500);
        let after_update = store.get_pending_remaining_ms(&id);
        assert!(after_update > 100);
        assert!(after_update <= 500);

        // (3) remaining is 0 after waiting past the TTL (real sleep)
        std::thread::sleep(Duration::from_millis(650));
        assert_eq!(store.get_pending_remaining_ms(&id), 0);

        // (4) update_pending_ttl is a no-op once the entry has expired
        store.update_pending_ttl(&id, 1_000);
        assert_eq!(store.get_pending_remaining_ms(&id), 0);

        // ...and a no-op after clear_pending
        store.try_set_pending(&id, 1_000);
        store.clear_pending(&id);
        store.update_pending_ttl(&id, 1_000);
        assert_eq!(store.get_pending_remaining_ms(&id), 0);
    }

    // ── multi-thread double-spend (inexpressible in the single-threaded ts suite) ──────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claims_yield_exactly_one_true() {
        const N: usize = 64;
        const ROUNDS: usize = 50;
        let id = default_id();

        for _ in 0..ROUNDS {
            let store = AuthorizationStore::new();
            store.grant(&id, 10_000);

            // An async barrier releases all N tasks together to maximize contention; a
            // blocking std::sync::Barrier across more tasks than worker threads would starve
            // the pool. claim() is synchronous and drops its guard before returning, so no
            // lock is ever held across an .await.
            let barrier = Arc::new(Barrier::new(N));
            let mut set = JoinSet::new();
            for _ in 0..N {
                let store = store.clone();
                let barrier = barrier.clone();
                let id = id.clone();
                set.spawn(async move {
                    barrier.wait().await;
                    store.claim(&id)
                });
            }

            let mut true_count = 0usize;
            while let Some(res) = set.join_next().await {
                if res.unwrap() {
                    true_count += 1;
                }
            }
            assert_eq!(
                true_count, 1,
                "exactly one claim must consume the single grant"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_try_set_pending_yields_exactly_one_true() {
        const N: usize = 64;
        const ROUNDS: usize = 50;
        let id = default_id();

        for _ in 0..ROUNDS {
            let store = AuthorizationStore::new();

            let barrier = Arc::new(Barrier::new(N));
            let mut set = JoinSet::new();
            for _ in 0..N {
                let store = store.clone();
                let barrier = barrier.clone();
                let id = id.clone();
                set.spawn(async move {
                    barrier.wait().await;
                    store.try_set_pending(&id, 10_000)
                });
            }

            let mut true_count = 0usize;
            while let Some(res) = set.join_next().await {
                if res.unwrap() {
                    true_count += 1;
                }
            }
            assert_eq!(
                true_count, 1,
                "exactly one try_set_pending must win the pending slot"
            );
        }
    }

    // ── Rust-specific: poison recovery, identity isolation, map independence ───────────

    #[test]
    fn poison_recovery() {
        let store = AuthorizationStore::new();
        let id = default_id();

        // Poison the inner mutex: a thread panics while holding the guard.
        let poison_store = store.clone();
        let handle = std::thread::spawn(move || {
            let _held = poison_store.inner.lock().unwrap();
            panic!("intentional panic while holding the lock (expected: poisons the mutex)");
        });
        assert!(handle.join().is_err()); // the panic was observed

        // The store still works: lock()'s into_inner() recovers the poisoned guard.
        store.grant(&id, 10_000);
        assert!(store.claim(&id));
    }

    #[test]
    fn distinct_identities_do_not_share_state() {
        // Axis 1: differ by client_pubkey (same invocation_hash).
        let store = AuthorizationStore::new();
        let id_a = id("client-a", "hash-1");
        let id_b = id("client-b", "hash-1");
        store.grant(&id_a, 10_000);
        assert!(!store.claim(&id_b)); // a different client has no grant
        assert!(store.claim(&id_a)); // the original grant is untouched

        // Axis 2: differ by invocation_hash (same client_pubkey).
        let store = AuthorizationStore::new();
        let id_a = id("client-1", "hash-a");
        let id_c = id("client-1", "hash-c");
        store.grant(&id_a, 10_000);
        assert!(!store.claim(&id_c)); // a different invocation has no grant
        assert!(store.claim(&id_a));
    }

    // ── the composed claim-or-set-pending entry sequence ───────────────────────────────

    #[test]
    fn claim_or_set_pending_claims_a_live_grant() {
        let store = AuthorizationStore::new();
        let id = default_id();

        store.grant(&id, 10_000);
        assert_eq!(
            store.claim_or_set_pending(&id, 10_000),
            ClaimOrPending::Claimed,
            "a live grant must be consumed, not re-offered"
        );
        // Single-use survives the composition: the second call finds no grant and
        // starts a fresh offer cycle.
        assert_eq!(
            store.claim_or_set_pending(&id, 10_000),
            ClaimOrPending::PendingSet
        );
    }

    #[test]
    fn claim_or_set_pending_parks_a_live_pending() {
        // Live half: a not-yet-expired assertion, so the TTL stays well above the
        // scheduler-stall flake floor (>= 100 ms).
        let store = AuthorizationStore::new();
        let id = default_id();
        assert_eq!(
            store.claim_or_set_pending(&id, 10_000),
            ClaimOrPending::PendingSet
        );
        match store.claim_or_set_pending(&id, 10_000) {
            ClaimOrPending::AlreadyPending { remaining_ms } => {
                assert!(remaining_ms > 0, "a live pending has time left");
                assert!(
                    remaining_ms <= 10_000,
                    "remaining cannot exceed the set TTL"
                );
            }
            other => panic!("expected AlreadyPending, got {other:?}"),
        }

        // Expired half: an expired-direction assertion, so a tiny TTL plus a real
        // sleep is flake-free (a sleep only oversleeps).
        let store = AuthorizationStore::new();
        assert_eq!(
            store.claim_or_set_pending(&id, 20),
            ClaimOrPending::PendingSet
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            store.claim_or_set_pending(&id, 10_000),
            ClaimOrPending::PendingSet,
            "an expired pending is overwritten, not parked behind"
        );
    }

    #[test]
    fn claim_or_set_pending_prefers_grant_over_pending() {
        // Populate BOTH maps: grant first (grant() clears pending for the key, so the
        // pending write must come second, simulating a second offer cycle in flight).
        let store = AuthorizationStore::new();
        let id = default_id();
        store.grant(&id, 10_000);
        assert!(store.try_set_pending(&id, 10_000));

        assert_eq!(
            store.claim_or_set_pending(&id, 10_000),
            ClaimOrPending::Claimed,
            "the grant wins over a coexisting pending (claim-then-pending order)"
        );
        assert!(
            store.get_pending_remaining_ms(&id) > 0,
            "the claim must not clear another cycle's pending"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_claim_or_set_pending_yields_one_claim() {
        const N: usize = 64;
        const ROUNDS: usize = 50;
        let id = default_id();

        for _ in 0..ROUNDS {
            let store = AuthorizationStore::new();
            store.grant(&id, 10_000);

            let barrier = Arc::new(Barrier::new(N));
            let mut set = JoinSet::new();
            for _ in 0..N {
                let store = store.clone();
                let barrier = barrier.clone();
                let id = id.clone();
                set.spawn(async move {
                    barrier.wait().await;
                    store.claim_or_set_pending(&id, 10_000)
                });
            }

            let (mut claimed, mut pending_set, mut already) = (0usize, 0usize, 0usize);
            while let Some(res) = set.join_next().await {
                match res.unwrap() {
                    ClaimOrPending::Claimed => claimed += 1,
                    ClaimOrPending::PendingSet => pending_set += 1,
                    ClaimOrPending::AlreadyPending { .. } => already += 1,
                }
            }
            assert_eq!(claimed, 1, "exactly one caller consumes the single grant");
            assert_eq!(pending_set, 1, "exactly one loser wins the pending slot");
            assert_eq!(already, N - 2, "everyone else parks behind the pending");
        }
    }

    // Real threads and real time (no async): two OS threads rendezvous on a reused
    // std barrier per round, which is what makes the interleaving genuinely parallel.
    #[test]
    fn grant_between_ops_cannot_be_missed() {
        // The composition invariant: with a live pending pre-set and one concurrent
        // grant, the composed op must NEVER answer PendingSet: at any serialization
        // point it sees either the live pending (parks) or the landed grant (claims).
        // A sequential claim-then-set-pending port loses this on the schedule where
        // the grant lands between its two critical sections (popping the pending on
        // the way in), minting a spurious second invoice while a paid grant sits
        // unclaimed. That window is nanoseconds wide, so this probe hammers it: one
        // shared store, a fresh identity per round, and a reused two-thread barrier
        // whose wake jitter scatters the granter across the caller's whole window.
        // Kill margin: against the sequential port this trips a handful of times per
        // 100k rounds (machine- and load-dependent), so it can never fail on correct
        // code but a single suspicious GREEN under a refactor of this operation is not
        // proof; re-run it a few times before trusting one.
        const ROUNDS: usize = 100_000;
        let store = AuthorizationStore::new();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let round_id = |i: usize| id("client", &format!("h{i}"));

        let granter = {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                for i in 0..ROUNDS {
                    let id = round_id(i);
                    barrier.wait();
                    store.grant(&id, 10_000);
                    barrier.wait();
                }
            })
        };

        let mut violations = Vec::new();
        for i in 0..ROUNDS {
            let id = round_id(i);
            assert!(store.try_set_pending(&id, 10_000));
            barrier.wait();
            let outcome = store.claim_or_set_pending(&id, 10_000);
            barrier.wait();
            match outcome {
                // The op ran after the grant: it consumed it, nothing is claimable.
                ClaimOrPending::Claimed => assert!(!store.claim(&id)),
                // The op ran before the grant (the live pending parked it): the grant
                // landed afterwards and must still be claimable, not shadowed.
                ClaimOrPending::AlreadyPending { .. } => assert!(store.claim(&id)),
                // The spurious-invoice cell: the grant popped the pending between the
                // two halves and the op started a fresh offer cycle anyway.
                ClaimOrPending::PendingSet => {
                    violations.push((i, store.claim(&id), store.get_pending_remaining_ms(&id)))
                }
            }
        }
        granter.join().expect("granter thread");
        assert!(
            violations.is_empty(),
            "the composed op must never answer PendingSet under a concurrent grant: {violations:?}"
        );
    }

    #[test]
    fn grant_and_pending_maps_evict_independently() {
        // Two independent cap-2 maps must hold all four entries at once. A single merged
        // cap-2 map would evict a grant (h1) on the pending write, failing claim(h1).
        let store = AuthorizationStore::with_max_entries(2);
        let h1 = id("client", "h1");
        let h2 = id("client", "h2");
        let p1 = id("client", "p1");

        store.grant(&h1, 10_000);
        store.grant(&h2, 10_000); // authorizations map now at cap
        store.try_set_pending(&p1, 10_000); // a write to the *pending* map

        assert!(
            store.get_pending_remaining_ms(&p1) > 0,
            "pending p1 must survive the grants being at cap"
        );
        assert!(store.claim(&h1), "grant h1 must survive the pending write");
        assert!(store.claim(&h2), "grant h2 must survive the pending write");
    }
}
