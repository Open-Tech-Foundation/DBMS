//! The single writer task.
//!
//! One thread owns the [`Pager`] and drains the write queue. It coalesces every
//! request currently waiting into one batch (**group commit**): each
//! transaction's [`WriteJob`] is applied to the evolving copy-on-write root,
//! then the whole batch is made durable with a single data fsync + meta swap +
//! meta fsync. Superseded pages are parked and only returned to the allocator
//! once the reclamation watermark proves no live snapshot can still see them.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use common::{CategorizedError, ErrorCategory, IoBackend};
use pager::{PageId, Pager};

use crate::job::{WriteCtx, WriteJob};
use crate::registry::Registry;
use crate::{Result, TxnError};

/// A queued write transaction and the channel to answer it on.
pub(crate) struct Request<B: IoBackend, J: WriteJob<B>> {
    /// The transaction to apply atomically.
    pub job: J,
    /// One-shot reply: the committed txn id and the job's output, or why the
    /// transaction failed.
    pub resp: Sender<Result<(u64, J::Out)>>,
}

/// A reply channel paired with how its transaction fared within the batch
/// (`Ok` replies are completed with the batch's committed txn id).
type PendingReply<B, J> = (
    Sender<Result<(u64, <J as WriteJob<B>>::Out)>>,
    Result<<J as WriteJob<B>>::Out>,
);

/// An error that stops the writer (vs. one that rejects a single transaction).
fn is_fatal(err: &TxnError) -> bool {
    matches!(
        err.category(),
        ErrorCategory::Io | ErrorCategory::Corruption
    )
}

/// The writer: owns the pager, the live root, and the parked free lists.
pub(crate) struct Writer<B: IoBackend> {
    pager: Arc<Pager<B>>,
    registry: Arc<Registry>,
    /// The latest (possibly uncommitted within a batch) published root.
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
    pub(crate) fn run<J: WriteJob<B>>(mut self, rx: Receiver<Request<B, J>>) {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            while let Ok(next) = rx.try_recv() {
                batch.push(next);
            }
            if self.process_batch(batch).is_err() {
                // A fatal error makes the database unwritable; stop. Pending
                // requests then see their reply channel close (WriterStopped).
                break;
            }
        }
    }

    fn process_batch<J: WriteJob<B>>(&mut self, batch: Vec<Request<B, J>>) -> Result<()> {
        // Return pages that no live snapshot can see to the allocator first, so
        // this batch can reuse them.
        self.reclaim()?;

        let mut replies: Vec<PendingReply<B, J>> = Vec::with_capacity(batch.len());
        let mut batch_freed: Vec<PageId> = Vec::new();
        let root_before = self.root;

        for req in batch {
            let root_at_start = self.root;
            let freed_at_start = batch_freed.len();
            let mut ctx = WriteCtx {
                pager: &self.pager,
                root: &mut self.root,
                freed: &mut batch_freed,
            };
            match req.job.apply(&mut ctx) {
                Ok(out) => replies.push((req.resp, Ok(out))),
                Err(fatal) if is_fatal(&fatal) => {
                    // Abort the whole batch without committing; nothing applied
                    // here was durable. Tell everyone, then stop the writer.
                    let _ = req.resp.send(Err(clone_fatal(&fatal)));
                    for (resp, _) in replies {
                        let _ = resp.send(Err(clone_fatal(&fatal)));
                    }
                    return Err(fatal);
                }
                Err(rejected) => {
                    // The transaction is a no-op: restore the pre-job state
                    // (defense in depth — a contract-abiding job already left
                    // both untouched).
                    self.root = root_at_start;
                    batch_freed.truncate(freed_at_start);
                    replies.push((req.resp, Err(rejected)));
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
            let msg = outcome.map(|out| (committed_txn, out));
            let _ = resp.send(msg);
        }
        Ok(())
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
