use lru::LruCache;
use std::num::NonZeroUsize;

const MIN_CACHE_SIZE: usize = 1;

/// Bounded seen-event cache used to suppress duplicate Nostr events.
pub(crate) struct SeenEventCache {
    cache: LruCache<String, ()>,
}

impl SeenEventCache {
    pub(crate) fn new(capacity: usize) -> Self {
        let bounded_capacity = capacity.max(MIN_CACHE_SIZE);
        let cache_size =
            NonZeroUsize::new(bounded_capacity).expect("bounded seen-event cache size is non-zero");
        Self {
            cache: LruCache::new(cache_size),
        }
    }

    /// Returns true when the event was already seen.
    pub(crate) fn is_duplicate(&mut self, event_id: &str) -> bool {
        self.cache.put(event_id.to_string(), ()).is_some()
    }
}

/// State holder used by transport loops to suppress duplicate events and track hits.
pub(crate) struct EventDeduplicator {
    seen: SeenEventCache,
    duplicate_hits: u64,
}

impl EventDeduplicator {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            seen: SeenEventCache::new(capacity),
            duplicate_hits: 0,
        }
    }

    /// Returns true when processing should stop because this event is a duplicate.
    pub(crate) fn should_drop(&mut self, event_id: &str) -> bool {
        if self.seen.is_duplicate(event_id) {
            self.duplicate_hits = self.duplicate_hits.saturating_add(1);
            true
        } else {
            false
        }
    }

    pub(crate) fn duplicate_hits(&self) -> u64 {
        self.duplicate_hits
    }
}

#[cfg(test)]
mod tests {
    use super::{EventDeduplicator, SeenEventCache};

    #[test]
    fn test_seen_event_cache_detects_duplicates() {
        let mut cache = SeenEventCache::new(16);
        assert!(!cache.is_duplicate("event-1"));
        assert!(cache.is_duplicate("event-1"));
    }

    #[test]
    fn test_seen_event_cache_evicts_oldest_when_full() {
        let mut cache = SeenEventCache::new(2);
        assert!(!cache.is_duplicate("a"));
        assert!(!cache.is_duplicate("b"));
        assert!(!cache.is_duplicate("c"));

        // "a" was the least-recently seen and should have been evicted.
        assert!(!cache.is_duplicate("a"));
    }

    #[test]
    fn test_seen_event_cache_minimum_capacity_is_one() {
        let mut cache = SeenEventCache::new(0);
        assert!(!cache.is_duplicate("x"));
        assert!(!cache.is_duplicate("y"));
        assert!(!cache.is_duplicate("x"));
    }

    #[test]
    fn test_event_deduplicator_counts_duplicate_hits() {
        let mut deduplicator = EventDeduplicator::new(8);

        assert!(!deduplicator.should_drop("evt-1"));
        assert_eq!(deduplicator.duplicate_hits(), 0);

        assert!(deduplicator.should_drop("evt-1"));
        assert_eq!(deduplicator.duplicate_hits(), 1);

        assert!(deduplicator.should_drop("evt-1"));
        assert_eq!(deduplicator.duplicate_hits(), 2);
    }

    #[test]
    fn test_event_deduplicator_simulates_multi_relay_duplicate_delivery() {
        let mut deduplicator = EventDeduplicator::new(16);
        let deliveries = [
            ("relay-a", "event-1"),
            ("relay-b", "event-1"),
            ("relay-c", "event-1"),
            ("relay-a", "event-2"),
            ("relay-b", "event-2"),
        ];

        let mut forwarded = Vec::new();
        for (_, event_id) in deliveries {
            if !deduplicator.should_drop(event_id) {
                forwarded.push(event_id);
            }
        }

        assert_eq!(forwarded, vec!["event-1", "event-2"]);
        assert_eq!(deduplicator.duplicate_hits(), 3);
    }

    #[test]
    fn test_event_deduplicator_blocks_recent_replay_within_window() {
        let mut deduplicator = EventDeduplicator::new(4);

        assert!(!deduplicator.should_drop("replay-me"));
        assert!(deduplicator.should_drop("replay-me"));
        assert_eq!(deduplicator.duplicate_hits(), 1);
    }
}
