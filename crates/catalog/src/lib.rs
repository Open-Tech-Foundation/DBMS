//! `catalog` — schema, system catalog, constraints.
//!
//! The schema model (tables/columns/types/PK/indexes/update policy/defaults/
//! generators/rowversion/on_update/checks), the in-file system catalog, DDL,
//! and constraint enforcement hooks. Implemented in Phase 6; this is the
//! Phase 1 scaffold (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

/// Errors raised by the catalog layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    /// An error from the underlying transaction layer.
    #[error(transparent)]
    Txn(#[from] txn::TxnError),
    /// An error from the type/encoding layer.
    #[error(transparent)]
    Type(#[from] types::TypeError),
}

impl CategorizedError for CatalogError {
    fn category(&self) -> ErrorCategory {
        match self {
            CatalogError::Txn(e) => e.category(),
            CatalogError::Type(e) => e.category(),
        }
    }
}

/// Result alias for catalog operations.
pub type Result<T> = std::result::Result<T, CatalogError>;
