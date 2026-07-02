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
    /// The **authoritative** set of free (allocatable) page ids, held in
    /// memory. `alloc`/`free` mutate only this; the durable trunk chain is a
    /// serialization of it, rebuilt into crash-safe pages at each `commit`
    /// (never mutated in place — that would corrupt the last committed meta's
    /// free-list on a crash between the data sync and the meta swap).
    free_set: Vec<u64>,
    /// The pages currently serving as the durable trunk chain (structural, not
    /// allocatable). They become free one commit after they go unreferenced.
    trunks: Vec<u64>,
    /// Whether `free_set`/`trunks` have been loaded from the durable trunk
    /// chain. Loading is deferred to the first write (`alloc`/`free`/`commit`)
    /// so a read-only open touches only the meta — and a database whose
    /// free-list is corrupt stays fully *readable* (the corruption blocks
    /// writes, and surfaces in `validate`, but never a plain read).
    free_loaded: bool,
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
                free_set: Vec::new(),
                trunks: Vec::new(),
                // A fresh database starts with an empty, already-loaded free set.
                free_loaded: true,
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
                // Deferred until the first write recovers it from the trunk chain.
                free_set: Vec::new(),
                trunks: Vec::new(),
                free_loaded: false,
            }),
            txn_id,
        })
    }

    /// Walk the committed free-list trunk chain, returning the free (allocatable)
    /// page ids it stores and the trunk page ids themselves.
    fn load_free_list(backend: &B, meta: &Meta) -> Result<(Vec<u64>, Vec<u64>)> {
        let mut free = Vec::new();
        let mut trunks = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut head = meta.freelist_head;
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
            let mut frame = page::zeroed();
            backend.read_at(trunk_id.get() * PAGE_SIZE as u64, &mut frame[..])?;
            page::verify(&frame[..], trunk_id)?;
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
            trunks.push(trunk_id.get());
            for i in 0..count {
                free.push(freelist::id_at(&frame, i));
            }
            let next = freelist::next(&frame);
            head = (next != 0).then(|| PageId::new(next));
        }
        Ok((free, trunks))
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

    /// Allocate a page id, reusing a free page when one is available, else
    /// extending the file's high-water mark.
    pub fn alloc(&self) -> Result<PageId> {
        let mut st = self.locked();
        self.ensure_free_loaded(&mut st)?;
        let id = match st.free_set.pop() {
            Some(id) => PageId::new(id),
            None => {
                let id = PageId::new(st.meta.page_count);
                st.meta.page_count += 1;
                id
            }
        };
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

    /// Return a page to the free set for later reuse. In-memory only; the change
    /// is serialized to the durable trunk chain at the next [`commit`](Self::commit).
    pub fn free(&self, id: PageId) -> Result<()> {
        let mut st = self.locked();
        self.in_range(&st, id)?;
        self.ensure_free_loaded(&mut st)?;
        st.free_set.push(id.get());
        Ok(())
    }

    /// Recover the free set from the durable trunk chain on first write. A
    /// corrupt trunk surfaces here (blocking the write) rather than at open.
    fn ensure_free_loaded(&self, st: &mut State) -> Result<()> {
        if !st.free_loaded {
            let (free_set, trunks) = Self::load_free_list(&self.backend, &st.meta)?;
            st.free_set = free_set;
            st.trunks = trunks;
            st.free_loaded = true;
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
        // 1. Under the lock, serialize the free set into a fresh (crash-safe)
        //    trunk chain, then snapshot what to flush and the meta to install.
        let (dirty, meta, inactive, needed, dead_trunks) = {
            let mut st = self.locked();
            self.ensure_free_loaded(&mut st)?;
            let dead_trunks = Self::rebuild_free_list(&mut st);
            let needed = st.meta.page_count * PAGE_SIZE as u64;
            let dirty = st.cache.dirty_pages();
            let inactive = 1 - st.active_slot;
            let mut meta = st.meta.clone();
            meta.txn_id += 1;
            (dirty, meta, inactive, needed, dead_trunks)
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

        // 3. Promote under the lock: only now is the commit visible. The
        //    previous trunk pages are now unreferenced by the durable meta, so
        //    they can rejoin the free set (they were kept out until this point
        //    so the pre-commit meta's free-list stayed intact for recovery).
        {
            let mut st = self.locked();
            st.cache.clear_dirty();
            st.meta = meta;
            st.active_slot = inactive;
            st.free_set.extend(dead_trunks);
            self.txn_id.store(st.meta.txn_id, Ordering::Release);
        }
        Ok(())
    }

    /// Serialize the in-memory free set into a fresh trunk chain, staged dirty
    /// in the cache, and point the (in-memory) meta at it. Container pages are
    /// drawn from the free set itself — their current content is free-page data
    /// the committed meta never reads, so overwriting them is crash-safe, and
    /// the previous trunk pages are left untouched on disk (so the last
    /// committed free-list survives a crash before the meta swap). Returns those
    /// now-superseded trunk pages for the caller to recycle after the swap.
    fn rebuild_free_list(st: &mut State) -> Vec<u64> {
        let dead_trunks = std::mem::take(&mut st.trunks);
        let mut pool = std::mem::take(&mut st.free_set);
        let mut new_trunks = Vec::new();
        let mut stored: Vec<u64> = Vec::new();
        let mut head: u64 = 0;
        // Each trunk container holds up to CAPACITY of the remaining free ids;
        // the container page itself is spent as structure, not stored as free.
        while let Some(container) = pool.pop() {
            let take = pool.len().min(freelist::CAPACITY as usize);
            let ids: Vec<u64> = pool.split_off(pool.len() - take);
            let mut frame = page::zeroed();
            freelist::set_next(&mut frame, head);
            freelist::set_count(&mut frame, take as u32);
            for (i, &id) in ids.iter().enumerate() {
                freelist::set_id_at(&mut frame, i as u32, id);
            }
            page::finalize(&mut frame, PageType::Freelist, PageId::new(container));
            st.cache.insert(container, Arc::from(frame), true);
            stored.extend(ids);
            head = container;
            new_trunks.push(container);
        }
        st.meta.freelist_head = (head != 0).then(|| PageId::new(head));
        st.meta.freelist_len = stored.len() as u64;
        st.free_set = stored;
        st.trunks = new_trunks;
        dead_trunks
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
