//! `query` — surface lowering, validator, planner, executor, write path.
//!
//! Phase 8 delivers the surface → IR [`lower`]ing: both surface forms fold
//! into one logical-plan tree (the clause form desugars into its fixed-order
//! pipeline first, so equivalence is by construction). The validator (names,
//! types, `SPEC.md` §6 safety rules), planner (index/join/stage choices),
//! executor (pull-based operators), write path (guarded read-check-write,
//! optimistic version), and EXPLAIN follow in Phase 9.

mod lower;

use common::{CategorizedError, ErrorCategory};

pub use lower::{lower, LowerError};

/// Errors raised by the query layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// An error from the protocol layer.
    #[error(transparent)]
    Proto(#[from] proto::ProtoError),
    /// A select that does not lower into the IR.
    #[error(transparent)]
    Lower(#[from] LowerError),
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
            QueryError::Lower(e) => e.category(),
            QueryError::Catalog(e) => e.category(),
            QueryError::Index(e) => e.category(),
            QueryError::Txn(e) => e.category(),
        }
    }
}

/// Result alias for query operations.
pub type Result<T> = std::result::Result<T, QueryError>;
