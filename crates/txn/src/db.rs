//! The database handle, write submission, and read snapshots.

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use btree::BTree;
use common::IoBackend;
use pager::{PageId, Pager};

use crate::registry::Registry;
use crate::writer::{Request, Writer};
use crate::{Op, Result, TxnError};

/// State shared between every [`Db`] clone and the writer thread.
struct Shared<B: IoBackend> {
    pager: Arc<Pager<B>>,
    registry: Arc<Registry>,
    /// The write queue's sending half; `None` once the handle is shutting down.
    sender: Mutex<Option<Sender<Request>>>,
    /// The writer thread, joined on shutdown.
    join: Mutex<Option<JoinHandle<()>>>,
}

/// An embedded database handle.
///
/// Cheap to [`clone`](Clone) — every clone shares one writer thread and one page
/// store. Writes are serialized through the writer (and batched into group
/// commits); reads run concurrently against pinned [`Snapshot`]s.
pub struct Db<B: IoBackend> {
    shared: Arc<Shared<B>>,
}

impl<B: IoBackend> Clone for Db<B> {
    fn clone(&self) -> Self {
        Db {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<B: IoBackend + 'static> Db<B> {
    /// Create a fresh database on an empty backend (initializes an empty data
    /// tree and commits it as transaction 1).
    pub fn create(backend: B) -> Result<Self> {
        let pager = Pager::create(backend)?;
        let root = BTree::new(&pager).create()?;
        pager.set_catalog_root(Some(root));
        pager.commit()?;
        let txn_id = pager.txn_id();
        Ok(Self::spawn(pager, root, txn_id))
    }

    /// Open an existing database, recovering to its last committed transaction.
    pub fn open(backend: B) -> Result<Self> {
        let pager = Pager::open(backend)?;
        let (root, txn_id) = match pager.catalog_root() {
            Some(root) => (root, pager.txn_id()),
            None => {
                // A pager made outside this layer has no data tree yet; seed one.
                let root = BTree::new(&pager).create()?;
                pager.set_catalog_root(Some(root));
                pager.commit()?;
                (root, pager.txn_id())
            }
        };
        Ok(Self::spawn(pager, root, txn_id))
    }

    fn spawn(pager: Pager<B>, root: PageId, txn_id: u64) -> Self {
        let pager = Arc::new(pager);
        let registry = Arc::new(Registry::new(Some(root), txn_id));
        let (sender, rx) = channel::<Request>();
        let writer = Writer::new(Arc::clone(&pager), Arc::clone(&registry), root);
        let join = std::thread::spawn(move || writer.run(rx));
        Db {
            shared: Arc::new(Shared {
                pager,
                registry,
                sender: Mutex::new(Some(sender)),
                join: Mutex::new(Some(join)),
            }),
        }
    }

    /// Apply a transaction (a sequence of [`Op`]s) atomically and durably,
    /// returning the committed transaction id. Blocks until the writer has made
    /// it durable.
    pub fn write(&self, ops: Vec<Op>) -> Result<u64> {
        let (resp, reply) = channel::<Result<u64>>();
        let req = Request { ops, resp };
        {
            let guard = lock(&self.shared.sender);
            match guard.as_ref() {
                Some(sender) => sender.send(req).map_err(|_| writer_gone())?,
                None => return Err(TxnError::Closed),
            }
        }
        reply.recv().map_err(|_| writer_gone())?
    }

    /// Insert or replace a single key/value.
    pub fn put(&self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Result<u64> {
        self.write(vec![Op::Put(key.into(), value.into())])
    }

    /// Delete a single key.
    pub fn delete(&self, key: impl Into<Vec<u8>>) -> Result<u64> {
        self.write(vec![Op::Delete(key.into())])
    }

    /// Pin the latest committed state as a consistent read snapshot.
    pub fn snapshot(&self) -> Snapshot<B> {
        let (root, txn_id) = self.shared.registry.acquire();
        Snapshot {
            pager: Arc::clone(&self.shared.pager),
            registry: Arc::clone(&self.shared.registry),
            root,
            txn_id,
        }
    }

    /// The latest committed transaction id.
    pub fn txn_id(&self) -> u64 {
        self.shared.registry.current().1
    }

    /// Run the pager's integrity check over the last committed state and
    /// return storage statistics (page count, free-list size, …).
    pub fn validate(&self) -> Result<pager::PagerStats> {
        Ok(self.shared.pager.validate()?)
    }
}

impl<B: IoBackend> Drop for Shared<B> {
    fn drop(&mut self) {
        // Closing the sender ends the writer's recv loop; then join it.
        *lock(&self.sender) = None;
        if let Some(handle) = lock(&self.join).take() {
            let _ = handle.join();
        }
    }
}

/// A consistent, immutable read view pinned at one committed version.
///
/// Holds its version live (against page reclamation) until dropped, so a long
/// read never sees a later writer's changes and never reads a recycled page.
pub struct Snapshot<B: IoBackend> {
    pager: Arc<Pager<B>>,
    registry: Arc<Registry>,
    root: Option<PageId>,
    txn_id: u64,
}

impl<B: IoBackend> Snapshot<B> {
    /// The transaction id this snapshot is pinned at.
    pub fn txn_id(&self) -> u64 {
        self.txn_id
    }

    /// Look up a key in this snapshot.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.root {
            None => Ok(None),
            Some(root) => Ok(BTree::new(&*self.pager).lookup(root, key)?),
        }
    }

    /// Collect `[lo, hi)` (each bound optional) in ascending key order.
    pub fn range(&self, lo: Option<&[u8]>, hi: Option<&[u8]>) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        match self.root {
            None => Ok(Vec::new()),
            Some(root) => Ok(BTree::new(&*self.pager)
                .range(root, lo, hi)?
                .collect_all()?),
        }
    }

    /// Collect the whole snapshot in ascending key order.
    pub fn scan(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.range(None, None)
    }
}

impl<B: IoBackend> Drop for Snapshot<B> {
    fn drop(&mut self) {
        self.registry.release(self.txn_id);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn writer_gone() -> TxnError {
    TxnError::WriterStopped {
        reason: "the writer thread has stopped".to_string(),
    }
}
