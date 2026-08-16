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
#[derive(Default)]
pub struct ListingCache {
    map: DashMap<RemotePath, CacheEntry>,
}

impl ListingCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &RemotePath) -> Option<CacheEntry> {
        self.map.get(key).map(|e| e.clone())
    }

    pub fn put(&self, key: RemotePath, entries: Vec<DirEntry>, complete: bool) {
        self.map.insert(
            key,
            CacheEntry {
                entries: Arc::new(entries),
                loaded_at: Instant::now(),
                complete,
            },
        );
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
