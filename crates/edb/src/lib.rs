//! `otf-edb` — the public embedded database API.
//!
//! Ties the engine together behind a small, hard-to-misuse surface: open or
//! create a [`Database`], run DDL, [`execute`](Database::execute) a request,
//! commit a [`transaction`](Database::transaction), page results through a
//! snapshot-owning [`Cursor`], and [`check`](Database::check) or
//! [`inspect`](Database::inspect) the file. Read results through [`Response`]
//! and [`Row`]'s typed accessors.
//!
//! A single public [`Error`] aggregates every layer and maps it to the
//! [`ErrorCategory`] taxonomy from `SPEC.md` §9.
//!
//! # Examples
//!
//! ```
//! use otf_edb::{ColumnDef, Database, Insert, Request, Select, Stage, TableDef, TableRef, TypeKind, Value};
//!
//! let db = Database::create_memory().unwrap();
//! db.create_table(TableDef::new(
//!     "greetings",
//!     vec![ColumnDef::new("id", TypeKind::I64), ColumnDef::new("text", TypeKind::Text)],
//!     vec!["id"],
//! ))
//! .unwrap();
//! db.execute(&Request::Insert(Insert {
//!     table: "greetings".into(),
//!     rows: vec![vec![("id".into(), Value::I64(1)), ("text".into(), Value::Text("hi".into()))]],
//! }))
//! .unwrap();
//!
//! let out = db
//!     .execute(&Request::Select(Select::Pipeline(vec![Stage::Scan(TableRef {
//!         table: "greetings".into(),
//!         alias: None,
//!     })])))
//!     .unwrap();
//! assert_eq!(out.row(0).unwrap().get_text("text").unwrap(), Some("hi"));
//! ```

// Compile the README's quick-start as a doctest so it can never rot.
#[doc = include_str!("../../../README.md")]
#[cfg(doctest)]
struct ReadmeDoctests;

mod cursor;
mod db;
mod inspect;
mod result;

use common::CategorizedError;

pub use common::ErrorCategory;
// The storage backend trait `Database` is generic over, plus the in-memory
// backend, so callers can name the `Database<B>` bound and build test/embedded
// instances through `otf_edb` alone.
pub use common::{IoBackend, MemoryBackend};

pub use cursor::Cursor;
pub use db::Database;
pub use inspect::{Inspection, IntegrityReport, TableInfo};
pub use result::{DecodeError, Response, Row};

// Re-export the schema, request, and value types so a caller can build and read
// everything through `otf_edb` alone. `catalog::CmpOp` (the CHECK comparison
// operator) is aliased to `CheckCmpOp` so it does not collide with the
// expression-grammar `proto::CmpOp` re-exported below.
pub use catalog::{
    CheckExpr, CmpOp as CheckCmpOp, ColumnDef, ForeignKey, IndexDef, RefAction, TableDef,
};
pub use proto::{
    AggFunc, ArithOp, ClauseSelect, CmpOp, Delete, Dir, Expr, Insert, JoinKind, JoinSpec,
    Projection, QueryResult, Request, Select, Selector, SortKey, Stage, TableRef, Update,
};
pub use types::{TypeKind, Value};

/// The single public error type. Every crate-local error folds into one of
/// these variants; [`Error::category`] reports its `SPEC.md` §9 category.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A low-level IO error (opening or sizing the backing file).
    #[error(transparent)]
    Io(#[from] common::IoError),
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
    /// Decoding a result cell as the wrong type or an unknown column.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// A misuse of the public API (e.g. creating over a non-empty file).
    #[error("{0}")]
    Usage(&'static str),
}

impl Error {
    /// The `SPEC.md` §9 taxonomy category of this error.
    ///
    /// # Examples
    ///
    /// ```
    /// use otf_edb::{Error, ErrorCategory};
    ///
    /// // A type error surfaces as a `Validation` category error.
    /// let err: Error = types::TypeError::BadUuid.into();
    /// assert_eq!(err.category(), ErrorCategory::Validation);
    /// ```
    pub fn category(&self) -> ErrorCategory {
        match self {
            Error::Io(e) => e.category(),
            Error::Pager(e) => e.category(),
            Error::BTree(e) => e.category(),
            Error::Txn(e) => e.category(),
            Error::Type(e) => e.category(),
            Error::Catalog(e) => e.category(),
            Error::Index(e) => e.category(),
            Error::Proto(e) => e.category(),
            Error::Query(e) => e.category(),
            Error::Decode(e) => e.category(),
            Error::Usage(_) => ErrorCategory::Validation,
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
        let err: Error = proto::ProtoError::Truncated.into();
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
