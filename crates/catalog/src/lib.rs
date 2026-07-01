//! `catalog` — schema, system catalog, constraints.
//!
//! Strict typed tables (`SPEC.md` §4) over the transaction layer: the schema
//! model ([`TableDef`]/[`ColumnDef`]), the in-file system catalog (stored in
//! the B+tree at the published root), DDL, and write-path constraint
//! enforcement (NOT NULL, UNIQUE, CHECK, DEFAULT, auto-increment, rowversion,
//! `on_update: now`).
//!
//! ```
//! use std::sync::Arc;
//! use catalog::{Catalog, ColumnDef, TableDef};
//! use common::{ManualClock, MemoryBackend, SeededRng};
//! use types::{TypeKind, Value};
//!
//! let cat = Catalog::create(
//!     MemoryBackend::new(),
//!     Arc::new(ManualClock::new(1_000_000)),
//!     Arc::new(SeededRng::new(7)),
//! )
//! .unwrap();
//!
//! cat.create_table(TableDef::new(
//!     "users",
//!     vec![
//!         ColumnDef::new("id", TypeKind::I64).auto_increment(),
//!         ColumnDef::new("name", TypeKind::Text).not_null(),
//!     ],
//!     vec!["id"],
//! ))
//! .unwrap();
//!
//! let row = cat
//!     .insert("users", vec![("name".into(), Value::Text("ada".into()))])
//!     .unwrap();
//! assert_eq!(row[0], Value::I64(1)); // generated key
//! ```

mod codec;
mod db;
mod job;
mod policy;
mod schema;
mod store;

use common::{CategorizedError, ErrorCategory};
use types::TypeKind;

pub use db::{CatSnapshot, Catalog};
pub use policy::{PolicyError, RowFilter, RowUpdater};
pub use schema::{
    implicit_index_name, CheckExpr, CmpOp, ColumnDef, DefaultSpec, IndexDef, TableDef, UpdatePolicy,
};

/// How stored catalog bytes can be malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CatalogCorruption {
    /// The bytes end mid-record.
    Truncated,
    /// An unknown format version.
    BadVersion {
        /// The version byte found.
        version: u8,
    },
    /// An unknown tag byte.
    BadTag {
        /// The offending tag.
        tag: u8,
    },
    /// An embedded value failed to decode.
    BadValue,
    /// A string is not valid UTF-8.
    InvalidUtf8,
    /// CHECK nesting beyond the stored-format limit.
    DepthExceeded,
    /// Bytes left over after the record.
    TrailingBytes,
    /// A required companion entry (table root) is missing.
    MissingEntry,
    /// A stored row has more cells than the schema has columns.
    RowWiderThanSchema,
}

impl std::fmt::Display for CatalogCorruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogCorruption::Truncated => write!(f, "catalog bytes are truncated"),
            CatalogCorruption::BadVersion { version } => {
                write!(f, "unknown catalog format version {version}")
            }
            CatalogCorruption::BadTag { tag } => write!(f, "unknown catalog tag {tag:#04x}"),
            CatalogCorruption::BadValue => write!(f, "embedded value failed to decode"),
            CatalogCorruption::InvalidUtf8 => write!(f, "catalog string is not valid UTF-8"),
            CatalogCorruption::DepthExceeded => write!(f, "stored check nests too deeply"),
            CatalogCorruption::TrailingBytes => write!(f, "trailing bytes after catalog record"),
            CatalogCorruption::MissingEntry => write!(f, "a table's root entry is missing"),
            CatalogCorruption::RowWiderThanSchema => {
                write!(f, "stored row is wider than the schema")
            }
        }
    }
}

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
    /// An error from the index layer.
    #[error(transparent)]
    Index(#[from] index::IndexError),
    /// Stored catalog bytes are corrupt.
    #[error("corrupt catalog: {0}")]
    Corrupt(CatalogCorruption),
    /// A schema definition is invalid (`SPEC.md` §4 rules).
    #[error("invalid schema: {reason}")]
    InvalidSchema {
        /// What rule the definition broke.
        reason: String,
    },
    /// `create table` for a name that already exists.
    #[error("table {table:?} already exists")]
    TableExists {
        /// The conflicting table name.
        table: String,
    },
    /// The named table does not exist.
    #[error("unknown table {table:?}")]
    UnknownTable {
        /// The missing table name.
        table: String,
    },
    /// The named column does not exist.
    #[error("unknown column {column:?} in table {table:?}")]
    UnknownColumn {
        /// The table searched.
        table: String,
        /// The missing column name.
        column: String,
    },
    /// The same column was provided twice in one row.
    #[error("column {column:?} provided more than once")]
    DuplicateColumn {
        /// The repeated column name.
        column: String,
    },
    /// An explicit write to an engine-managed column (rowversion,
    /// `on_update: now`).
    #[error("column {column:?} is engine-managed and cannot be written")]
    EngineManagedColumn {
        /// The engine-managed column.
        column: String,
    },
    /// An update tried to change a primary-key column (immutable in v1).
    #[error("primary-key column {column:?} cannot be updated")]
    PkImmutable {
        /// The PK column.
        column: String,
    },
    /// No row exists at the given primary key.
    #[error("no row with that key in table {table:?}")]
    RowNotFound {
        /// The table searched.
        table: String,
    },
    /// A NULL reached a NOT NULL column.
    #[error("column {column:?} of table {table:?} is not nullable")]
    NotNull {
        /// The table written.
        table: String,
        /// The NOT NULL column.
        column: String,
    },
    /// A value's type does not match the column's declared type.
    #[error("column {column:?} of {table:?} is {expected}, got {found}")]
    TypeMismatch {
        /// The table written.
        table: String,
        /// The mistyped column.
        column: String,
        /// The declared type.
        expected: TypeKind,
        /// What arrived instead.
        found: String,
    },
    /// A primary-key collision.
    #[error("duplicate primary key in table {table:?}")]
    DuplicateKey {
        /// The table written.
        table: String,
    },
    /// A unique-index collision (including the implicit index backing a
    /// `unique` column).
    #[error("unique violation on index {index:?} of table {table:?}")]
    UniqueViolation {
        /// The table written.
        table: String,
        /// The violated unique index.
        index: String,
    },
    /// `create index` for a name that already exists on the table.
    #[error("index {index:?} already exists on table {table:?}")]
    IndexExists {
        /// The table.
        table: String,
        /// The conflicting index name.
        index: String,
    },
    /// The named index does not exist on the table.
    #[error("unknown index {index:?} on table {table:?}")]
    UnknownIndex {
        /// The table searched.
        table: String,
        /// The missing index name.
        index: String,
    },
    /// An index disagrees with its base table (corruption or a maintenance
    /// bug), found by [`CatSnapshot::validate`].
    #[error("index {index:?} of table {table:?} is out of sync with its base")]
    IndexOutOfSync {
        /// The table.
        table: String,
        /// The inconsistent index.
        index: String,
    },
    /// A CHECK constraint evaluated to false.
    #[error("check constraint #{index} of table {table:?} violated")]
    CheckViolation {
        /// The table written.
        table: String,
        /// Which check (by definition order).
        index: usize,
    },
    /// A rejection raised by a caller-supplied write policy (e.g. the query
    /// layer's expression evaluator). Its taxonomy category is preserved; a
    /// higher layer can downcast `source` to recover the original typed error.
    #[error("{source}")]
    Policy {
        /// The category the policy reported.
        category: ErrorCategory,
        /// The boxed original error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl CategorizedError for CatalogError {
    fn category(&self) -> ErrorCategory {
        match self {
            CatalogError::Txn(e) => e.category(),
            CatalogError::Type(e) => e.category(),
            CatalogError::Index(e) => e.category(),
            CatalogError::Corrupt(_) => ErrorCategory::Corruption,
            CatalogError::InvalidSchema { .. }
            | CatalogError::TableExists { .. }
            | CatalogError::UnknownColumn { .. }
            | CatalogError::DuplicateColumn { .. }
            | CatalogError::EngineManagedColumn { .. }
            | CatalogError::PkImmutable { .. } => ErrorCategory::Validation,
            CatalogError::UnknownTable { .. }
            | CatalogError::RowNotFound { .. }
            | CatalogError::UnknownIndex { .. } => ErrorCategory::NotFound,
            CatalogError::IndexExists { .. } => ErrorCategory::Validation,
            CatalogError::IndexOutOfSync { .. } => ErrorCategory::Corruption,
            CatalogError::NotNull { .. }
            | CatalogError::TypeMismatch { .. }
            | CatalogError::DuplicateKey { .. }
            | CatalogError::UniqueViolation { .. }
            | CatalogError::CheckViolation { .. } => ErrorCategory::Constraint,
            CatalogError::Policy { category, .. } => *category,
        }
    }
}

/// Result alias for catalog operations.
pub type Result<T> = std::result::Result<T, CatalogError>;
