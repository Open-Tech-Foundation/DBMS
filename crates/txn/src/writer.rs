//! The single writer task.
//!
//! One thread owns the [`Pager`] and drains the write queue. It coalesces every
//! request currently waiting into one batch (**group commit**): each transaction
//! is applied to the evolving copy-on-write root, then the whole batch is made
//! durable with a single data fsync + meta swap + meta fsync. Superseded pages
//! are parked and only returned to the allocator once the reclamation watermark
//! proves no live snapshot can still see them.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use btree::BTree;
use common::IoBackend;
use pager::{PageId, Pager};

use crate::registry::Registry;
use crate::{Op, Result, TxnError};

/// A queued write transaction and the channel to answer it on.
pub(crate) struct Request {
    /// The operations to apply atomically.
    pub ops: Vec<Op>,
    /// One-shot reply: the committed txn id, or why the transaction failed.
    pub resp: Sender<Result<u64>>,
}

/// A reply channel paired with how its transaction fared within the batch
/// (`Ok` replies are completed with the batch's committed txn id).
type PendingReply = (Sender<Result<u64>>, std::result::Result<(), TxnError>);

/// How applying one transaction turned out.
enum Applied {
    /// Applied; the root may or may not have changed.
    Ok,
    /// Rejected by validation before any mutation (transaction is a no-op).
    Rejected(TxnError),
}

/// The writer: owns the pager, the live root, and the parked free lists.
pub(crate) struct Writer<B: IoBackend> {
    pager: Arc<Pager<B>>,
    registry: Arc<Registry>,
    /// The latest (possibly uncommitted within a batch) data root.
    root: PageId,
    /// Pages superseded by commit `T`, awaiting reclamation: `(T, pages)`.
    pending: Vec<(u64, Vec<PageId>)>,
}

impl<B: IoBackend> Writer<B> {
    pub(crate) fn new(pager: Arc<Pager<B>>, registry: Arc<Registry>, root: PageId) -> Self {
        Writer {
            pager,
            registry,
            root,
            pending: Vec::new(),
        }
    }

    /// Drain the queue until every sender is gone, batching greedily.
    pub(crate) fn run(mut self, rx: Receiver<Request>) {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            while let Ok(next) = rx.try_recv() {
                batch.push(next);
            }
            if self.process_batch(batch).is_err() {
                // A fatal I/O error makes the database unwritable; stop. Pending
                // requests then see their reply channel close (WriterStopped).
                break;
            }
        }
    }

    fn process_batch(&mut self, batch: Vec<Request>) -> Result<()> {
        // Return pages that no live snapshot can see to the allocator first, so
        // this batch can reuse them.
        self.reclaim()?;

        let mut replies: Vec<PendingReply> = Vec::with_capacity(batch.len());
        let mut batch_freed: Vec<PageId> = Vec::new();
        let root_before = self.root;

        for req in batch {
            match self.apply(&req.ops, &mut batch_freed) {
                Ok(Applied::Ok) => replies.push((req.resp, Ok(()))),
                Ok(Applied::Rejected(e)) => replies.push((req.resp, Err(e))),
                Err(fatal) => {
                    // Abort the whole batch without committing; nothing applied
                    // here was durable. Tell everyone, then stop the writer.
                    let _ = req.resp.send(Err(clone_fatal(&fatal)));
                    for (resp, _) in replies {
                        let _ = resp.send(Err(clone_fatal(&fatal)));
                    }
                    return Err(fatal);
                }
            }
        }

        // CoW only ever moves the root to a fresh page, so an unmoved root means
        // nothing changed and there is nothing to commit.
        let committed_txn = if self.root != root_before {
            self.pager.set_catalog_root(Some(self.root));
            if let Err(fatal) = self.pager.commit() {
                let fatal = TxnError::from(fatal);
                for (resp, _) in replies {
                    let _ = resp.send(Err(clone_fatal(&fatal)));
                }
                return Err(fatal);
            }
            let new_txn = self.pager.txn_id();
            if !batch_freed.is_empty() {
                self.pending.push((new_txn, batch_freed));
            }
            self.registry.publish(Some(self.root), new_txn);
            new_txn
        } else {
            // Nothing changed (e.g. only deletes of absent keys); report the
            // current committed version.
            self.pager.txn_id()
        };

        for (resp, outcome) in replies {
            let msg = outcome.map(|()| committed_txn);
            let _ = resp.send(msg);
        }
        Ok(())
    }

    /// Apply one transaction atomically. Validation failures reject the whole
    /// transaction before any mutation; I/O failures are fatal.
    fn apply(&mut self, ops: &[Op], batch_freed: &mut Vec<PageId>) -> Result<Applied> {
        // Pre-validate every op so a transaction never mutates then fails.
        for op in ops {
            if let Op::Put(key, value) = op {
                if let Err(e) = btree::check_entry(key, value) {
                    return Ok(Applied::Rejected(e.into()));
                }
            }
        }

        let tree = BTree::new(&*self.pager);
        let mut root = self.root;
        for op in ops {
            let edit = match op {
                Op::Put(key, value) => tree.insert(root, key, value)?,
                Op::Delete(key) => tree.delete(root, key)?,
            };
            batch_freed.extend(edit.freed);
            root = edit.new_root;
        }
        self.root = root;
        Ok(Applied::Ok)
    }

    /// Return parked pages to the allocator once no live snapshot can see them.
    fn reclaim(&mut self) -> Result<()> {
        let watermark = self.registry.watermark();
        let mut still = Vec::new();
        for (txn, pages) in std::mem::take(&mut self.pending) {
            if txn <= watermark {
                for page in pages {
                    self.pager.free(page)?;
                }
            } else {
                still.push((txn, pages));
            }
        }
        self.pending = still;
        Ok(())
    }
}

/// Reconstruct a fatal error to fan out to a whole batch. The variants we treat
/// as fatal here carry no owned payload that prevents a faithful copy.
fn clone_fatal(err: &TxnError) -> TxnError {
    TxnError::WriterStopped {
        reason: err.to_string(),
    }
}
