//! `btree` — copy-on-write B+tree.
//!
//! The ordered map underlying tables and indexes: point lookup, range scans,
//! insert/delete, node split/merge, and root-handle semantics (old roots stay
//! valid for live snapshots). Implemented in Phase 3; this is the Phase 1
//! scaffold (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

/// Errors raised by the B+tree.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BTreeError {
    /// An error from the underlying pager.
    #[error(transparent)]
    Pager(#[from] pager::PagerError),
}

impl CategorizedError for BTreeError {
    fn category(&self) -> ErrorCategory {
        match self {
            BTreeError::Pager(e) => e.category(),
        }
    }
}

/// Result alias for B+tree operations.
pub type Result<T> = std::result::Result<T, BTreeError>;
