use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::{DirEntry, RemotePath};

#[derive(Clone)]
pub struct CacheEntry {
    /// Shared with every pane displaying this directory. Never cloned to sort —
    /// see `sort_indices`.
    pub entries: Arc<Vec<DirEntry>>,
    pub loaded_at: Instant,
    /// `false` while batches are still arriving.
    pub complete: bool,
}

/// Directory listings kept so that revisiting a directory paints immediately
/// while a fresh listing runs in the background.
///
/// Bounded by the number of *entries* held rather than the number of
/// directories: one bucket prefix with 200k objects costs as much as two hundred
/// ordinary folders, and it was the large ones that made an unbounded cache hurt.
/// Before this it never evicted anything, so a session that browsed a lot of big
/// directories kept every one of them resident until it quit.
#[derive(Default)]
pub struct ListingCache {
    map: DashMap<RemotePath, CacheEntry>,
}

impl ListingCache {
    /// Roughly 20 MB of `DirEntry` plus their strings. Generous enough that
    /// ordinary browsing never evicts, small enough that a few huge listings
    /// cannot sit on hundreds of megabytes.
    pub const MAX_ENTRIES: usize = 250_000;

    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries are held across every cached directory.
    pub fn total_entries(&self) -> usize {
        self.map.iter().map(|e| e.entries.len()).sum()
    }

    /// Drop the least recently loaded directories until the budget is met.
    ///
    /// The directory just inserted is never a candidate — evicting what the user
    /// is looking at would make navigating away and back re-list every time.
    fn evict_until_within_budget(&self, keep: &RemotePath) {
        let mut held = self.total_entries();
        if held <= Self::MAX_ENTRIES {
            return;
        }

        // Oldest first. Collected up front because removing while iterating a
        // DashMap shard can deadlock.
        let mut candidates: Vec<(RemotePath, Instant, usize)> = self
            .map
            .iter()
            .filter(|e| e.key() != keep)
            .map(|e| (e.key().clone(), e.loaded_at, e.entries.len()))
            .collect();
        candidates.sort_by_key(|(_, loaded_at, _)| *loaded_at);

        for (key, _, len) in candidates {
            if held <= Self::MAX_ENTRIES {
                break;
            }
            if self.map.remove(&key).is_some() {
                held = held.saturating_sub(len);
            }
        }
    }

    pub fn get(&self, key: &RemotePath) -> Option<CacheEntry> {
        self.map.get(key).map(|e| e.clone())
    }

    pub fn put(&self, key: RemotePath, entries: Vec<DirEntry>, complete: bool) {
        self.map.insert(
            key.clone(),
            CacheEntry {
                entries: Arc::new(entries),
                loaded_at: Instant::now(),
                complete,
            },
        );
        self.evict_until_within_budget(&key);
    }

    pub fn invalidate(&self, key: &RemotePath) {
        self.map.remove(key);
    }

    /// Drop everything for one session, e.g. when its connection is closed.
    pub fn invalidate_session(&self, session: crate::SessionId) {
        self.map.retain(|k, _| k.session != session);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntryKind;

    fn entry(name: &str) -> DirEntry {
        DirEntry {
            name: name.into(),
            path: name.into(),
            kind: EntryKind::File,
            size: Some(1),
            modified: None,
            etag: None,
            meta_complete: true,
        }
    }

    #[test]
    fn round_trips_a_listing() {
        let cache = ListingCache::new();
        let key = RemotePath::new(0, "a/");

        assert!(cache.get(&key).is_none());
        cache.put(key.clone(), vec![entry("x")], true);

        let got = cache.get(&key).unwrap();
        assert_eq!(got.entries.len(), 1);
        assert!(got.complete);
    }

    #[test]
    fn sessions_are_invalidated_independently() {
        let cache = ListingCache::new();
        cache.put(RemotePath::new(0, "a/"), vec![entry("x")], true);
        cache.put(RemotePath::new(1, "a/"), vec![entry("y")], true);

        cache.invalidate_session(0);

        assert!(cache.get(&RemotePath::new(0, "a/")).is_none());
        assert!(cache.get(&RemotePath::new(1, "a/")).is_some());
    }

    #[test]
    fn same_path_in_different_sessions_does_not_collide() {
        let cache = ListingCache::new();
        cache.put(RemotePath::new(0, ""), vec![entry("local")], true);
        cache.put(RemotePath::new(1, ""), vec![entry("remote")], true);

        assert_eq!(cache.len(), 2);
        assert_eq!(
            &*cache.get(&RemotePath::new(0, "")).unwrap().entries[0].name,
            "local"
        );
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;
    use crate::{EntryKind, RemotePath};

    fn listing(n: usize) -> Vec<DirEntry> {
        (0..n)
            .map(|i| DirEntry {
                name: format!("f{i}").into(),
                path: format!("f{i}").into(),
                kind: EntryKind::File,
                size: None,
                modified: None,
                etag: None,
                meta_complete: true,
            })
            .collect()
    }

    /// Half the budget each, so the third insert has to evict.
    fn half() -> usize {
        ListingCache::MAX_ENTRIES / 2 + 1
    }

    #[test]
    fn browsing_does_not_grow_without_limit() {
        let cache = ListingCache::new();
        for d in 0..5 {
            cache.put(
                RemotePath::new(1, format!("dir-{d}/")),
                listing(half()),
                true,
            );
        }

        assert!(
            cache.total_entries() <= ListingCache::MAX_ENTRIES,
            "held {}",
            cache.total_entries()
        );
    }

    #[test]
    fn the_directory_just_loaded_is_never_the_one_evicted() {
        // Otherwise navigating into a big directory would immediately drop it and
        // re-list on the way back — the cache would be worse than none.
        let cache = ListingCache::new();
        cache.put(RemotePath::new(1, "old/"), listing(half()), true);

        let current = RemotePath::new(1, "current/");
        cache.put(current.clone(), listing(half()), true);

        assert!(
            cache.get(&current).is_some(),
            "the current directory survived"
        );
    }

    #[test]
    fn eviction_takes_the_least_recently_loaded_first() {
        let cache = ListingCache::new();
        let third = ListingCache::MAX_ENTRIES / 3 + 1;

        cache.put(RemotePath::new(1, "oldest/"), listing(third), true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        cache.put(RemotePath::new(1, "middle/"), listing(third), true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        cache.put(RemotePath::new(1, "newest/"), listing(third), true);

        // Three thirds fit; a fourth does not.
        std::thread::sleep(std::time::Duration::from_millis(2));
        cache.put(RemotePath::new(1, "current/"), listing(third), true);

        assert!(
            cache.get(&RemotePath::new(1, "oldest/")).is_none(),
            "oldest goes"
        );
        assert!(cache.get(&RemotePath::new(1, "current/")).is_some());
    }

    #[test]
    fn a_small_amount_of_browsing_never_evicts() {
        // The bound must not cost anything in ordinary use.
        let cache = ListingCache::new();
        for d in 0..50 {
            cache.put(RemotePath::new(1, format!("dir-{d}/")), listing(500), true);
        }

        assert_eq!(cache.len(), 50, "nothing should have been dropped");
    }
}
