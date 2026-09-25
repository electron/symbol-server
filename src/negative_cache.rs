//! A byte-bounded, TTL'd LRU set of upstream paths known to be missing,
//! matching the semantics of the Node server's `lru-cache@6` instance.

use lru::LruCache;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Total budget for the cache, charged per entry as key length plus
/// [`ENTRY_OVERHEAD_BYTES`].
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// Per-entry overhead charged against [`MAX_BYTES`]. This is the figure
/// measured for the Node implementation's heap cost per entry; it is kept
/// so the cache holds the same number of entries (~70k at typical key
/// lengths). The Rust entries are smaller, so real memory stays well under
/// the budget.
pub const ENTRY_OVERHEAD_BYTES: usize = 384;

/// How long a "this symbol does not exist" answer stays valid.
pub const TTL: Duration = Duration::from_secs(60 * 60);

pub struct NegativeCache {
    inner: Mutex<Inner>,
    max_bytes: usize,
    ttl: Duration,
}

struct Inner {
    entries: LruCache<String, Entry>,
    bytes: usize,
}

struct Entry {
    stored_at: Instant,
    cost: usize,
}

impl Default for NegativeCache {
    fn default() -> Self {
        Self::new(MAX_BYTES, TTL)
    }
}

impl NegativeCache {
    pub fn new(max_bytes: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: LruCache::unbounded(),
                bytes: 0,
            }),
            max_bytes,
            ttl,
        }
    }

    /// Whether `key` is a known miss. Like `lru-cache`'s `get`, a hit
    /// refreshes the entry's recency (not its age) and a stale entry is
    /// dropped.
    pub fn contains(&self, key: &str) -> bool {
        self.contains_at(key, Instant::now())
    }

    /// Records `key` as missing, evicting least-recently-used entries until
    /// the cache is back under budget. Keys that alone exceed the budget are
    /// not stored.
    pub fn insert(&self, key: String) {
        self.insert_at(key, Instant::now());
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock can't leave the cache in a state
        // that is unsafe to keep using, so ignore poisoning.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn contains_at(&self, key: &str, now: Instant) -> bool {
        let mut inner = self.lock();
        let Some(entry) = inner.entries.get(key) else {
            return false;
        };
        if now.saturating_duration_since(entry.stored_at) > self.ttl {
            if let Some(stale) = inner.entries.pop(key) {
                inner.bytes -= stale.cost;
            }
            return false;
        }
        true
    }

    fn insert_at(&self, key: String, now: Instant) {
        let cost = key.len() + ENTRY_OVERHEAD_BYTES;
        let mut inner = self.lock();
        if cost > self.max_bytes {
            if let Some(old) = inner.entries.pop(&key) {
                inner.bytes -= old.cost;
            }
            return;
        }
        if let Some(old) = inner.entries.put(
            key,
            Entry {
                stored_at: now,
                cost,
            },
        ) {
            inner.bytes -= old.cost;
        }
        inner.bytes += cost;
        while inner.bytes > self.max_bytes {
            match inner.entries.pop_lru() {
                Some((_, evicted)) => inner.bytes -= evicted.cost,
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(key: &str) -> usize {
        key.len() + ENTRY_OVERHEAD_BYTES
    }

    #[test]
    fn stores_and_finds_misses() {
        let cache = NegativeCache::default();
        assert!(!cache.contains("/a"));
        cache.insert("/a".into());
        assert!(cache.contains("/a"));
        assert!(!cache.contains("/b"));
        assert_eq!(cache.bytes(), cost("/a"));
    }

    #[test]
    fn evicts_least_recently_used_by_bytes() {
        // Room for exactly three 2-byte keys.
        let cache = NegativeCache::new(cost("/a") * 3, TTL);
        cache.insert("/a".into());
        cache.insert("/b".into());
        cache.insert("/c".into());
        // Touch /a so /b becomes the least recently used.
        assert!(cache.contains("/a"));
        cache.insert("/d".into());
        assert!(cache.contains("/a"));
        assert!(!cache.contains("/b"));
        assert!(cache.contains("/c"));
        assert!(cache.contains("/d"));
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.bytes(), cost("/a") * 3);
    }

    #[test]
    fn longer_keys_cost_more() {
        let cache = NegativeCache::new(cost("/a") * 2, TTL);
        cache.insert("/a".into());
        cache.insert("/b".into());
        // One byte longer than two short keys can make room for alongside it.
        cache.insert("/cc".into());
        assert_eq!(cache.len(), 1);
        assert!(cache.contains("/cc"));
    }

    #[test]
    fn entries_expire_after_ttl() {
        let cache = NegativeCache::new(MAX_BYTES, Duration::from_secs(60));
        let start = Instant::now();
        cache.insert_at("/a".into(), start);
        assert!(cache.contains_at("/a", start + Duration::from_secs(60)));
        assert!(!cache.contains_at("/a", start + Duration::from_secs(61)));
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn reinserting_refreshes_age_without_double_counting() {
        let cache = NegativeCache::new(MAX_BYTES, Duration::from_secs(60));
        let start = Instant::now();
        cache.insert_at("/a".into(), start);
        cache.insert_at("/a".into(), start + Duration::from_secs(50));
        assert!(cache.contains_at("/a", start + Duration::from_secs(100)));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), cost("/a"));
    }

    #[test]
    fn oversized_keys_are_not_stored() {
        let cache = NegativeCache::new(ENTRY_OVERHEAD_BYTES + 4, TTL);
        cache.insert("/abc".into());
        assert!(cache.contains("/abc"));
        cache.insert("/abcd".into());
        assert!(!cache.contains("/abcd"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn default_budget_holds_about_seventy_thousand_typical_keys() {
        let cache = NegativeCache::default();
        let key = |i: usize| format!("/electron.exe.pdb/{i:0>40}/electron.exe.pdb{:0>40}", "");
        for i in 0..100_000 {
            cache.insert(key(i));
        }
        let per_entry = cost(&key(0));
        assert_eq!(cache.len(), MAX_BYTES / per_entry);
        assert!(cache.bytes() <= MAX_BYTES);
    }
}
