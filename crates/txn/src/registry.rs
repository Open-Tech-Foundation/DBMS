//! The snapshot/version registry: the concurrency-critical handoff between the
//! single writer and the lock-free readers.
//!
//! It holds the latest published `(root, txn_id)` and a reference count of live
//! read snapshots keyed by the version they pinned. From those counts it derives
//! the **reclamation watermark** — the oldest version any live reader can still
//! see — which the writer uses to decide when a superseded page may be returned
//! to the allocator. A page freed by commit `T` is safe to reuse only once no
//! live snapshot older than `T` remains (watermark ≥ `T`).
//!
//! The `Mutex` is `loom`'s under `--cfg loom`, so the model checker can explore
//! every interleaving of `acquire`/`release`/`publish`.

use std::collections::BTreeMap;

#[cfg(loom)]
use loom::sync::Mutex;
#[cfg(not(loom))]
use std::sync::Mutex;

use pager::PageId;

/// A watermark meaning "no live reader constrains reclamation" — everything
/// pending may be reclaimed.
pub const UNBOUNDED: u64 = u64::MAX;

struct Inner {
    /// The latest committed data root (None for an empty database).
    root: Option<PageId>,
    /// The latest committed transaction id.
    txn_id: u64,
    /// Live read snapshots, keyed by the version they pinned → refcount.
    live: BTreeMap<u64, usize>,
}

/// Shared version state used by the writer (to publish) and readers (to pin and
/// release snapshots).
pub struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    /// Create a registry seeded with the database's opening version.
    pub fn new(root: Option<PageId>, txn_id: u64) -> Self {
        Registry {
            inner: Mutex::new(Inner {
                root,
                txn_id,
                live: BTreeMap::new(),
            }),
        }
    }

    // A poisoned registry lock cannot leave torn state (every critical
    // section is a few infallible map/field updates), so recover the guard.
    #[cfg(not(loom))]
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(loom)]
    fn lock(&self) -> loom::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    /// Publish a freshly committed version. Called by the writer after a commit.
    pub fn publish(&self, root: Option<PageId>, txn_id: u64) {
        let mut inner = self.lock();
        inner.root = root;
        inner.txn_id = txn_id;
    }

    /// The latest committed `(root, txn_id)` without pinning a snapshot.
    pub fn current(&self) -> (Option<PageId>, u64) {
        let inner = self.lock();
        (inner.root, inner.txn_id)
    }

    /// Pin the latest committed version as a read snapshot, returning its
    /// `(root, txn_id)`. Must be paired with one [`release`](Self::release).
    pub fn acquire(&self) -> (Option<PageId>, u64) {
        let mut inner = self.lock();
        let txn_id = inner.txn_id;
        let root = inner.root;
        *inner.live.entry(txn_id).or_insert(0) += 1;
        (root, txn_id)
    }

    /// Release a snapshot previously pinned at `txn_id`.
    pub fn release(&self, txn_id: u64) {
        let mut inner = self.lock();
        if let Some(count) = inner.live.get_mut(&txn_id) {
            *count -= 1;
            if *count == 0 {
                inner.live.remove(&txn_id);
            }
        }
    }

    /// The reclamation watermark: the oldest version any live snapshot can see,
    /// or [`UNBOUNDED`] when there are no live snapshots.
    pub fn watermark(&self) -> u64 {
        let inner = self.lock();
        inner.live.keys().next().copied().unwrap_or(UNBOUNDED)
    }

    /// The number of live snapshots (test/diagnostic helper).
    pub fn live_count(&self) -> usize {
        self.lock().live.values().sum()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn watermark_tracks_oldest_live_snapshot() {
        let reg = Registry::new(Some(PageId::new(2)), 1);
        assert_eq!(reg.watermark(), UNBOUNDED);

        let (_r, a) = reg.acquire(); // pins v1
        assert_eq!(a, 1);
        assert_eq!(reg.watermark(), 1);

        reg.publish(Some(PageId::new(3)), 2);
        let (_r, b) = reg.acquire(); // pins v2
        assert_eq!(b, 2);
        // Oldest live is still v1.
        assert_eq!(reg.watermark(), 1);

        reg.release(1);
        assert_eq!(reg.watermark(), 2);
        reg.release(2);
        assert_eq!(reg.watermark(), UNBOUNDED);
    }

    #[test]
    fn refcounts_share_a_version() {
        let reg = Registry::new(None, 5);
        reg.acquire();
        reg.acquire();
        assert_eq!(reg.live_count(), 2);
        assert_eq!(reg.watermark(), 5);
        reg.release(5);
        assert_eq!(reg.watermark(), 5); // one still live
        reg.release(5);
        assert_eq!(reg.watermark(), UNBOUNDED);
    }
}
