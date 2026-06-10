//! `txn` — transactions, MVCC, durability.
//!
//! A single writer thread owns the pager and applies write transactions one
//! batch at a time (group commit: many queued transactions, one fsync pair).
//! Readers take reference-counted [`Snapshot`]s pinned at a committed version;
//! a page a writer supersedes is returned to the allocator only once no live
//! snapshot can still see it (deferred, watermark-driven reclamation). Crash
//! recovery rides on the pager's double-buffered meta.
//!
//! ```
//! use common::MemoryBackend;
//! use txn::Db;
//!
//! let db = Db::create(MemoryBackend::new()).unwrap();
//! db.put(b"k".to_vec(), b"v".to_vec()).unwrap();
//!
//! let snap = db.snapshot();
//! assert_eq!(snap.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
//! ```

mod db;
mod job;
mod registry;
mod writer;

use common::{CategorizedError, ErrorCategory};

pub use db::{Db, JobDb, Snapshot};
pub use job::{OpsJob, WriteCtx, WriteJob};
pub use registry::{Registry, UNBOUNDED};

/// One mutation within a write transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Insert or replace `key` → `value`.
    Put(Vec<u8>, Vec<u8>),
    /// Remove `key` if present.
    Delete(Vec<u8>),
}

/// Errors raised by the transaction layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TxnError {
    /// An error from the underlying pager.
    #[error(transparent)]
    Pager(#[from] pager::PagerError),
    /// An error from the underlying B+tree.
    #[error(transparent)]
    BTree(#[from] btree::BTreeError),
    /// A write job rejected the transaction before mutating anything — a
    /// validation or constraint failure raised by a higher layer (e.g. the
    /// catalog). Carries that layer's typed error; its category is the inner
    /// error's category.
    #[error("transaction rejected: {0}")]
    Rejected(Box<dyn CategorizedError + Send + Sync>),
    /// The writer thread has stopped (after a fatal I/O error, or shutdown); the
    /// database is no longer writable.
    #[error("writer stopped: {reason}")]
    WriterStopped {
        /// What stopped the writer.
        reason: String,
    },
    /// The handle was used after it began shutting down.
    #[error("database handle is closed")]
    Closed,
}

impl CategorizedError for TxnError {
    fn category(&self) -> ErrorCategory {
        match self {
            TxnError::Pager(e) => e.category(),
            TxnError::BTree(e) => e.category(),
            TxnError::Rejected(e) => e.category(),
            TxnError::WriterStopped { .. } | TxnError::Closed => ErrorCategory::Io,
        }
    }
}

/// Result alias for transaction operations.
pub type Result<T> = std::result::Result<T, TxnError>;
