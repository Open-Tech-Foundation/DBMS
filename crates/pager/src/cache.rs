//! A byte-budgeted page cache with LRU eviction.
//!
//! Entries are reference-counted ([`Arc`]) page frames. Dirty frames are pinned
//! until the next commit flushes them, so eviction only reclaims clean entries;
//! if every entry is dirty the cache may temporarily exceed its budget.
//!
//! Eviction currently scans for the least-recently-used clean entry (O(n)); the
//! working sets here are small and this is not yet on a measured hot path. An
//! O(1) intrusive LRU is a Phase 11 optimization candidate.

use std::collections::HashMap;
use std::sync::Arc;

use crate::page::{Frame, PAGE_SIZE};

struct Entry {
    frame: Arc<Frame>,
    dirty: bool,
    tick: u64,
}

pub(crate) struct PageCache {
    entries: HashMap<u64, Entry>,
    budget_bytes: usize,
    tick: u64,
}

impl PageCache {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        PageCache {
            entries: HashMap::new(),
            budget_bytes,
            tick: 0,
        }
    }

    fn bump(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }

    /// Fetch a cached frame, marking it most-recently-used.
    pub(crate) fn get(&mut self, id: u64) -> Option<Arc<Frame>> {
        let tick = self.bump();
        let entry = self.entries.get_mut(&id)?;
        entry.tick = tick;
        Some(Arc::clone(&entry.frame))
    }

    /// Insert or replace a frame. A `dirty` insert pins the entry until the
    /// next commit; re-inserting never clears an existing dirty flag.
    pub(crate) fn insert(&mut self, id: u64, frame: Arc<Frame>, dirty: bool) {
        let tick = self.bump();
        match self.entries.get_mut(&id) {
            Some(entry) => {
                entry.frame = frame;
                entry.tick = tick;
                entry.dirty = entry.dirty || dirty;
            }
            None => {
                self.entries.insert(id, Entry { frame, dirty, tick });
            }
        }
        self.evict_to_budget();
    }

    fn evict_to_budget(&mut self) {
        while self.entries.len().saturating_mul(PAGE_SIZE) > self.budget_bytes {
            let victim = self
                .entries
                .iter()
                .filter(|(_, e)| !e.dirty)
                .min_by_key(|(_, e)| e.tick)
                .map(|(id, _)| *id);
            match victim {
                Some(id) => {
                    self.entries.remove(&id);
                }
                None => break, // everything resident is dirty; keep it
            }
        }
    }

    /// All currently-dirty frames, for the commit flush. Does not clear flags.
    pub(crate) fn dirty_pages(&self) -> Vec<(u64, Arc<Frame>)> {
        self.entries
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(id, e)| (*id, Arc::clone(&e.frame)))
            .collect()
    }

    /// Mark every entry clean (after a successful commit flush).
    pub(crate) fn clear_dirty(&mut self) {
        for entry in self.entries.values_mut() {
            entry.dirty = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, id: u64) -> bool {
        self.entries.contains_key(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page;

    fn frame(id: u64) -> Arc<Frame> {
        let mut f = page::zeroed();
        page::finalize(&mut f, page::PageType::Data, crate::PageId::new(id));
        Arc::from(f)
    }

    #[test]
    fn evicts_clean_lru_first() {
        // Budget for two pages.
        let mut cache = PageCache::new(2 * PAGE_SIZE);
        cache.insert(2, frame(2), false);
        cache.insert(3, frame(3), false);
        let _ = cache.get(2); // touch 2 → 3 is now LRU
        cache.insert(4, frame(4), false); // over budget → evict 3
        assert!(cache.contains(2));
        assert!(cache.contains(4));
        assert!(!cache.contains(3));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn dirty_pages_are_pinned_over_budget() {
        let mut cache = PageCache::new(PAGE_SIZE); // budget for one page
        cache.insert(2, frame(2), true);
        cache.insert(3, frame(3), true);
        // Both dirty → neither evicted even though over budget.
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.dirty_pages().len(), 2);
        cache.clear_dirty();
        // Now clean; next insert can evict down to budget.
        cache.insert(4, frame(4), false);
        assert_eq!(cache.len(), 1);
    }
}
