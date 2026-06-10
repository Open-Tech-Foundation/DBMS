//! `otf-dbms` — the public embedded database API.
//!
//! Ties the engine together behind a small, hard-to-misuse surface (open,
//! execute, transaction, cursor) and exposes the single public error type that
//! aggregates every layer and maps it to the [`ErrorCategory`] taxonomy from
//! `SPEC.md` §9. The functional API arrives in Phase 10; this is the Phase 1
//! scaffold (error aggregation only).

use common::CategorizedError;

pub use common::ErrorCategory;

/// The single public error type. Every crate-local error folds into one of
/// these variants; [`Error::category`] reports its `SPEC.md` §9 category.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An error from the pager.
    #[error(transparent)]
    Pager(#[from] pager::PagerError),
    /// An error from the B+tree.
    #[error(transparent)]
    BTree(#[from] btree::BTreeError),
    /// An error from the transaction layer.
    #[error(transparent)]
    Txn(#[from] txn::TxnError),
    /// An error from the type/encoding layer.
    #[error(transparent)]
    Type(#[from] types::TypeError),
    /// An error from the catalog layer.
    #[error(transparent)]
    Catalog(#[from] catalog::CatalogError),
    /// An error from the index layer.
    #[error(transparent)]
    Index(#[from] index::IndexError),
    /// An error from the protocol layer.
    #[error(transparent)]
    Proto(#[from] proto::ProtoError),
    /// An error from the query layer.
    #[error(transparent)]
    Query(#[from] query::QueryError),
}

impl Error {
    /// The `SPEC.md` §9 taxonomy category of this error.
    ///
    /// # Examples
    ///
    /// ```
    /// use otf_dbms::{Error, ErrorCategory};
    ///
    /// // A type error surfaces as a `Validation` category error.
    /// let err: Error = types::TypeError::BadUuid.into();
    /// assert_eq!(err.category(), ErrorCategory::Validation);
    /// ```
    pub fn category(&self) -> ErrorCategory {
        match self {
            Error::Pager(e) => e.category(),
            Error::BTree(e) => e.category(),
            Error::Txn(e) => e.category(),
            Error::Type(e) => e.category(),
            Error::Catalog(e) => e.category(),
            Error::Index(e) => e.category(),
            Error::Proto(e) => e.category(),
            Error::Query(e) => e.category(),
        }
    }
}

impl CategorizedError for Error {
    fn category(&self) -> ErrorCategory {
        Error::category(self)
    }
}

/// Result alias for the public API.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_propagate_as_io_category() {
        let io = common::IoError::OutOfBounds {
            offset: 0,
            requested: 1,
            size: 0,
        };
        let err: Error = pager::PagerError::from(io).into();
        assert_eq!(err.category(), ErrorCategory::Io);
    }

    #[test]
    fn protocol_malformed_is_validation() {
        let err: Error = proto::ProtoError::Malformed.into();
        assert_eq!(err.category(), ErrorCategory::Validation);
    }

    #[test]
    fn nested_propagation_preserves_category() {
        // query → txn → btree → pager → io, all the way down.
        let io = common::IoError::Backend(std::io::Error::other("disk gone"));
        let err: Error = query::QueryError::from(txn::TxnError::from(btree::BTreeError::from(
            pager::PagerError::from(io),
        )))
        .into();
        assert_eq!(err.category(), ErrorCategory::Io);
    }
}
