//! The pager: paged, checksummed, atomically-committing access to an
//! [`IoBackend`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use common::IoBackend;

use crate::cache::PageCache;
use crate::freelist;
use crate::meta::Meta;
use crate::page::{self, Frame, PageType, PAGE_SIZE};
use crate::{corrupt, CorruptionKind, PageId, PagerError, Result, FIRST_DATA_PAGE};

/// Default page-cache budget (1 MiB) when not otherwise specified.
pub const DEFAULT_CACHE_BYTES: usize = 1024 * 1024;

struct State {
    cache: PageCache,
    meta: Meta,
    /// The meta slot (0 or 1) currently holding committed state.
    active_slot: u64,
    /// When `Some`, every [`alloc`](Pager::alloc) records the page it hands
    /// out here. The writer uses this to reclaim the pages a rejected
    /// transaction allocated (they are unpublished, so freeing them is safe).
    alloc_log: Option<Vec<PageId>>,
}

/// Paged access to a single backing store.
///
/// Construct with [`Pager::create`] (fresh file) or [`Pager::open`] (existing).
/// Mutations ([`alloc`](Self::alloc), [`free`](Self::free),
/// [`write_page`](Self::write_page)) are staged in the cache and made durable
/// atomically by [`commit`](Self::commit) via the double-buffered meta swap.
pub struct Pager<B: IoBackend> {
    backend: B,
    state: Mutex<State>,
    /// Cheap, lock-free read of the committed transaction id.
    txn_id: AtomicU64,
}

impl<B: IoBackend> Pager<B> {
    /// Create a fresh database on an empty backend, with a 1 MiB cache.
    pub fn create(backend: B) -> Result<Self> {
        Self::create_with_cache(backend, DEFAULT_CACHE_BYTES)
    }

    /// Create a fresh database, specifying the page-cache byte budget.
    pub fn create_with_cache(backend: B, cache_bytes: usize) -> Result<Self> {
        let meta = Meta::initial();
        // Reserve both meta slots on disk.
        backend.truncate(FIRST_DATA_PAGE * PAGE_SIZE as u64)?;
        // Write the initial meta into slot 0; slot 1 stays zeroed (invalid) so
        // the first commit writes slot 1.
        let mut frame = page::zeroed();
        meta.encode(&mut frame);
        page::finalize(&mut frame, PageType::Meta, PageId::new(0));
        backend.write_at(0, &frame[..])?;
        backend.sync()?;
        let txn_id = AtomicU64::new(meta.txn_id);
        Ok(Pager {
            backend,
            state: Mutex::new(State {
                cache: PageCache::new(cache_bytes),
                meta,
                active_slot: 0,
                alloc_log: None,
            }),
            txn_id,
        })
    }

    /// Open an existing database, with a 1 MiB cache.
    pub fn open(backend: B) -> Result<Self> {
        Self::open_with_cache(backend, DEFAULT_CACHE_BYTES)
    }

    /// Open an existing database, specifying the page-cache byte budget.
    ///
    /// Both meta slots are validated by checksum; the valid slot with the
    /// highest committed transaction id is adopted (ties favour slot 0).
    pub fn open_with_cache(backend: B, cache_bytes: usize) -> Result<Self> {
        if backend.len()? < FIRST_DATA_PAGE * PAGE_SIZE as u64 {
            return Err(corrupt(CorruptionKind::NoValidMeta));
        }
        let slot0 = Self::read_meta(&backend, 0);
        let slot1 = Self::read_meta(&backend, 1);
        let (meta, active_slot) = match (slot0, slot1) {
            (Ok(a), Ok(b)) => {
                if b.txn_id > a.txn_id {
                    (b, 1)
                } else {
                    (a, 0)
                }
            }
            (Ok(a), Err(_)) => (a, 0),
            (Err(_), Ok(b)) => (b, 1),
            (Err(_), Err(_)) => return Err(corrupt(CorruptionKind::NoValidMeta)),
        };
        let txn_id = AtomicU64::new(meta.txn_id);
        Ok(Pager {
            backend,
            state: Mutex::new(State {
                cache: PageCache::new(cache_bytes),
                meta,
                active_slot,
                alloc_log: None,
            }),
            txn_id,
        })
    }

    /// The transaction id of the last successful commit.
    pub fn txn_id(&self) -> u64 {
        self.txn_id.load(Ordering::Acquire)
    }

    /// The current root of the system catalog, if set.
    pub fn catalog_root(&self) -> Option<PageId> {
        self.locked().meta.catalog_root
    }

    /// Stage a new catalog root for the next commit.
    pub fn set_catalog_root(&self, root: Option<PageId>) {
        self.locked().meta.catalog_root = root;
    }

    /// Read a data page (id ≥ 2), verifying its checksum and self-id.
    pub fn read_page(&self, id: PageId) -> Result<Arc<Frame>> {
        let mut st = self.locked();
        self.in_range(&st, id)?;
        self.fetch(&mut st, id)
    }

    /// Stage a full-page write of `payload` (with the given page type). The
    /// page must already be allocated (id < `page_count`).
    pub fn write_page(&self, id: PageId, page_type: PageType, payload: &[u8]) -> Result<()> {
        let mut st = self.locked();
        self.in_range(&st, id)?;
        let mut frame = page::zeroed();
        page::set_payload(&mut frame, payload)?;
        page::finalize(&mut frame, page_type, id);
        st.cache.insert(id.get(), Arc::from(frame), true);
        Ok(())
    }

    /// Allocate a page id, reusing a freed page when one is available, else
    /// extending the file's high-water mark.
    pub fn alloc(&self) -> Result<PageId> {
        let mut st = self.locked();
        let id = self.alloc_locked(&mut st)?;
        if let Some(log) = &mut st.alloc_log {
            log.push(id);
        }
        Ok(id)
    }

    /// Begin recording every page [`alloc`](Self::alloc) hands out, discarding
    /// any prior recording. The writer calls this before applying a transaction
    /// so a rejected one's allocations can be reclaimed.
    pub fn begin_alloc_recording(&self) {
        self.locked().alloc_log = Some(Vec::new());
    }

    /// Stop recording allocations and return the pages recorded since
    /// [`begin_alloc_recording`](Self::begin_alloc_recording).
    pub fn take_alloc_recording(&self) -> Vec<PageId> {
        self.locked().alloc_log.take().unwrap_or_default()
    }

    /// Return a page to the free list for later reuse.
    pub fn free(&self, id: PageId) -> Result<()> {
        let mut st = self.locked();
        self.in_range(&st, id)?;
        match st.meta.freelist_head {
            Some(head) => {
                let trunk = self.fetch(&mut st, head)?;
                let count = freelist::count(&trunk);
                if count < freelist::CAPACITY {
                    let mut next = *trunk;
                    freelist::set_id_at(&mut next, count, id.get());
                    freelist::set_count(&mut next, count + 1);
                    page::finalize(&mut next, PageType::Freelist, head);
                    st.cache.insert(head.get(), Arc::from(Box::new(next)), true);
                    st.meta.freelist_len += 1;
                } else {
                    // Head trunk is full: the freed page becomes a new head
                    // trunk chained to the old head.
                    self.init_trunk(&mut st, id, head.get());
                    st.meta.freelist_head = Some(id);
                }
            }
            None => {
                self.init_trunk(&mut st, id, 0);
                st.meta.freelist_head = Some(id);
            }
        }
        Ok(())
    }

    /// Flush staged changes durably and atomically: write dirty pages → fsync →
    /// write the new meta to the inactive slot → fsync → promote.
    ///
    /// The state lock is held only to snapshot the work and to promote the
    /// result — **never across the fsyncs** — so concurrent readers are not
    /// blocked during the durability window. This assumes a single writer (the
    /// `txn` layer serializes commits); concurrent commits are not supported.
    pub fn commit(&self) -> Result<()> {
        // 1. Under the lock, snapshot what to flush and the meta to install.
        let (dirty, meta, inactive, needed) = {
            let st = self.locked();
            let needed = st.meta.page_count * PAGE_SIZE as u64;
            let dirty = st.cache.dirty_pages();
            let inactive = 1 - st.active_slot;
            let mut meta = st.meta.clone();
            meta.txn_id += 1;
            (dirty, meta, inactive, needed)
        };

        // 2. Durable I/O with the lock released. Data pages → fsync → new meta
        //    to the inactive slot → fsync (the durability point).
        if self.backend.len()? < needed {
            self.backend.truncate(needed)?;
        }
        for (id, frame) in &dirty {
            self.backend.write_at(id * PAGE_SIZE as u64, &frame[..])?;
        }
        self.backend.sync()?;

        let mut frame = page::zeroed();
        meta.encode(&mut frame);
        page::finalize(&mut frame, PageType::Meta, PageId::new(inactive));
        self.backend
            .write_at(inactive * PAGE_SIZE as u64, &frame[..])?;
        self.backend.sync()?;

        // 3. Promote under the lock: only now is the commit visible.
        {
            let mut st = self.locked();
            st.cache.clear_dirty();
            st.meta = meta;
            st.active_slot = inactive;
            self.txn_id.store(st.meta.txn_id, Ordering::Release);
        }
        Ok(())
    }

    /// Walk the committed structures and prove their consistency (meta slots,
    /// free-list integrity, page-id ranges). Reflects the last committed state
    /// on disk; run after [`commit`](Self::commit).
    pub fn validate(&self) -> Result<PagerStats> {
        let st = self.locked();
        let meta = &st.meta;

        // The active meta slot must read back valid from disk.
        let on_disk = Self::read_meta(&self.backend, st.active_slot)?;
        if on_disk.txn_id != meta.txn_id {
            return Err(corrupt(CorruptionKind::BadMeta {
                slot: st.active_slot,
            }));
        }

        // Walk the free-list trunk chain.
        let mut seen = std::collections::HashSet::new();
        let mut head = meta.freelist_head;
        let mut stored_ids = 0u64;
        let mut trunks = 0u64;
        while let Some(trunk_id) = head {
            if trunk_id.get() < FIRST_DATA_PAGE || trunk_id.get() >= meta.page_count {
                return Err(corrupt(CorruptionKind::FreelistOutOfRange {
                    id: trunk_id.get(),
                }));
            }
            if !seen.insert(trunk_id.get()) {
                return Err(corrupt(CorruptionKind::FreelistCycle {
                    id: trunk_id.get(),
                }));
            }
            trunks += 1;
            let frame = self.read_committed(trunk_id)?;
            if page::page_type(&frame)? != PageType::Freelist {
                return Err(corrupt(CorruptionKind::FreelistBadType {
                    id: trunk_id.get(),
                }));
            }
            let count = freelist::count(&frame);
            if count > freelist::CAPACITY {
                return Err(corrupt(CorruptionKind::FreelistBadCount {
                    id: trunk_id.get(),
                }));
            }
            for i in 0..count {
                let entry = freelist::id_at(&frame, i);
                if entry < FIRST_DATA_PAGE || entry >= meta.page_count {
                    return Err(corrupt(CorruptionKind::FreelistOutOfRange { id: entry }));
                }
                if !seen.insert(entry) {
                    return Err(corrupt(CorruptionKind::FreelistDuplicate { id: entry }));
                }
                stored_ids += 1;
            }
            let next = freelist::next(&frame);
            head = if next == 0 {
                None
            } else {
                Some(PageId::new(next))
            };
        }
        if stored_ids != meta.freelist_len {
            return Err(corrupt(CorruptionKind::FreelistLenMismatch {
                counted: stored_ids,
                recorded: meta.freelist_len,
            }));
        }

        Ok(PagerStats {
            page_count: meta.page_count,
            free_ids: stored_ids,
            trunk_count: trunks,
            active_slot: st.active_slot,
            txn_id: meta.txn_id,
        })
    }

    /// Consume the pager and return the backend (for reopen-in-tests, etc.).
    pub fn into_backend(self) -> B {
        self.backend
    }

    // --- internals -------------------------------------------------------

    fn locked(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn in_range(&self, st: &State, id: PageId) -> Result<()> {
        if id.get() < FIRST_DATA_PAGE || id.get() >= st.meta.page_count {
            return Err(PagerError::PageOutOfRange {
                id: id.get(),
                page_count: st.meta.page_count,
            });
        }
        Ok(())
    }

    /// Fetch a frame from cache, or read+verify it from the backend.
    fn fetch(&self, st: &mut State, id: PageId) -> Result<Arc<Frame>> {
        if let Some(frame) = st.cache.get(id.get()) {
            return Ok(frame);
        }
        let mut frame = page::zeroed();
        self.backend
            .read_at(id.get() * PAGE_SIZE as u64, &mut frame[..])?;
        page::verify(&frame[..], id)?;
        let arc: Arc<Frame> = Arc::from(frame);
        st.cache.insert(id.get(), Arc::clone(&arc), false);
        Ok(arc)
    }

    fn alloc_locked(&self, st: &mut State) -> Result<PageId> {
        match st.meta.freelist_head {
            None => {
                let id = PageId::new(st.meta.page_count);
                st.meta.page_count += 1;
                Ok(id)
            }
            Some(head) => {
                let trunk = self.fetch(st, head)?;
                let count = freelist::count(&trunk);
                if count > 0 {
                    let id = freelist::id_at(&trunk, count - 1);
                    let mut next = *trunk;
                    freelist::set_count(&mut next, count - 1);
                    page::finalize(&mut next, PageType::Freelist, head);
                    st.cache.insert(head.get(), Arc::from(Box::new(next)), true);
                    st.meta.freelist_len -= 1;
                    Ok(PageId::new(id))
                } else {
                    // Empty trunk: hand out the trunk page itself.
                    let next = freelist::next(&trunk);
                    st.meta.freelist_head = if next == 0 {
                        None
                    } else {
                        Some(PageId::new(next))
                    };
                    Ok(head)
                }
            }
        }
    }

    fn init_trunk(&self, st: &mut State, id: PageId, next: u64) {
        let mut frame = page::zeroed();
        freelist::set_next(&mut frame, next);
        freelist::set_count(&mut frame, 0);
        page::finalize(&mut frame, PageType::Freelist, id);
        st.cache.insert(id.get(), Arc::from(frame), true);
    }

    /// Read a committed page straight from the backend (bypassing the cache),
    /// for `validate()`.
    fn read_committed(&self, id: PageId) -> Result<Box<Frame>> {
        let mut frame = page::zeroed();
        self.backend
            .read_at(id.get() * PAGE_SIZE as u64, &mut frame[..])?;
        page::verify(&frame[..], id)?;
        Ok(frame)
    }

    fn read_meta(backend: &B, slot: u64) -> Result<Meta> {
        let mut frame = page::zeroed();
        backend.read_at(slot * PAGE_SIZE as u64, &mut frame[..])?;
        let verified = page::verify(&frame[..], PageId::new(slot))?;
        if page::page_type(verified)? != PageType::Meta {
            return Err(corrupt(CorruptionKind::BadMeta { slot }));
        }
        Meta::decode(verified, PageId::new(slot))
    }
}

/// A summary of the pager's committed structure, returned by
/// [`Pager::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagerStats {
    /// Allocation high-water mark (total pages accounted for).
    pub page_count: u64,
    /// Free page ids stored in trunks.
    pub free_ids: u64,
    /// Number of free-list trunk pages.
    pub trunk_count: u64,
    /// The active meta slot (0 or 1).
    pub active_slot: u64,
    /// The committed transaction id.
    pub txn_id: u64,
}
