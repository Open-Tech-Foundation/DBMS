//! `query` — validator, planner, executor, write path.
//!
//! Lowers both surfaces to the IR, validates (names, types, `SPEC.md` §6
//! safety rules), plans (index/join/stage choices), executes (pull-based
//! operators), runs the write path (guarded read-check-write, optimistic
//! version), and produces EXPLAIN. Implemented in Phase 9; this is the
//! Phase 1 scaffold (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

/// Errors raised by the query layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// An error from the protocol layer.
    #[error(transparent)]
    Proto(#[from] proto::ProtoError),
    /// An error from the catalog layer.
    #[error(transparent)]
    Catalog(#[from] catalog::CatalogError),
    /// An error from the index layer.
    #[error(transparent)]
    Index(#[from] index::IndexError),
    /// An error from the transaction layer.
    #[error(transparent)]
    Txn(#[from] txn::TxnError),
}

impl CategorizedError for QueryError {
    fn category(&self) -> ErrorCategory {
        match self {
            QueryError::Proto(e) => e.category(),
            QueryError::Catalog(e) => e.category(),
            QueryError::Index(e) => e.category(),
            QueryError::Txn(e) => e.category(),
        }
    }
}

/// Result alias for query operations.
pub type Result<T> = std::result::Result<T, QueryError>;
