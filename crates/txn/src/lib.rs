//! `txn` — transactions, MVCC, durability.
//!
//! The single-writer queue, reference-counted read snapshots, the commit
//! pipeline (write pages → fsync → meta swap → fsync), group commit, page
//! reclamation, and crash recovery. Implemented in Phase 4; this is the
//! Phase 1 scaffold (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

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
}

impl CategorizedError for TxnError {
    fn category(&self) -> ErrorCategory {
        match self {
            TxnError::Pager(e) => e.category(),
            TxnError::BTree(e) => e.category(),
        }
    }
}

/// Result alias for transaction operations.
pub type Result<T> = std::result::Result<T, TxnError>;
