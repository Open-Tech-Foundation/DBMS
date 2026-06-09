//! `index` — secondary index maintenance.
//!
//! Secondary B+tree indexes (single-column, composite, unique) keyed by the
//! order-preserving encoding, kept atomic with base-row writes, with
//! unique-violation detection. Implemented in Phase 7; this is the Phase 1
//! scaffold (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

/// Errors raised by the index layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IndexError {
    /// An error from the underlying transaction layer.
    #[error(transparent)]
    Txn(#[from] txn::TxnError),
    /// An error from the type/encoding layer.
    #[error(transparent)]
    Type(#[from] types::TypeError),
}

impl CategorizedError for IndexError {
    fn category(&self) -> ErrorCategory {
        match self {
            IndexError::Txn(e) => e.category(),
            IndexError::Type(e) => e.category(),
        }
    }
}

/// Result alias for index operations.
pub type Result<T> = std::result::Result<T, IndexError>;
