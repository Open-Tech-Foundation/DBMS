//! Result-decoding helpers: typed, by-name access to a query's output.
//!
//! [`Response`] wraps the wire-level [`proto::QueryResult`] with an ergonomic,
//! misuse-resistant read surface — columns by name, rows as views, and typed
//! cell accessors that separate *null* (`Ok(None)`) from *wrong type* or
//! *unknown column* (a typed [`DecodeError`]). This is the "result-decoding
//! helpers" deliverable of Phase 10.

use common::{CategorizedError, ErrorCategory};
use proto::QueryResult;
use types::Value;

/// A failure decoding a cell from a [`Response`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// No output column has this name.
    #[error("unknown column `{0}`")]
    UnknownColumn(String),
    /// The cell is present and non-null but holds a different type.
    #[error("column `{column}` is not {expected}")]
    TypeMismatch {
        /// The column that was accessed.
        column: String,
        /// The Rust type the accessor expected.
        expected: &'static str,
    },
}

impl CategorizedError for DecodeError {
    fn category(&self) -> ErrorCategory {
        // Misreading a result is a caller/validation mistake, not a data fault.
        ErrorCategory::Validation
    }
}

/// The outcome of a query: output columns, rows, and any write/pagination
/// metadata. Returned by [`crate::Database::execute`] and
/// [`crate::Cursor::fetch`].
///
/// # Examples
///
/// ```
/// use otf_dbms::{ColumnDef, Database, Insert, Request, Select, Stage, TableRef, TypeKind, Value};
///
/// let db = Database::create_memory().unwrap();
/// db.create_table(otf_dbms::TableDef::new(
///     "t",
///     vec![ColumnDef::new("id", TypeKind::I64), ColumnDef::new("name", TypeKind::Text)],
///     vec!["id"],
/// ))
/// .unwrap();
/// db.execute(&Request::Insert(Insert {
///     table: "t".into(),
///     rows: vec![vec![("id".into(), Value::I64(1)), ("name".into(), Value::Text("Ada".into()))]],
/// }))
/// .unwrap();
///
/// let out = db
///     .execute(&Request::Select(Select::Pipeline(vec![Stage::Scan(TableRef {
///         table: "t".into(),
///         alias: None,
///     })])))
///     .unwrap();
/// assert_eq!(out.columns(), &["id", "name"]);
/// let row = out.row(0).unwrap();
/// assert_eq!(row.get_i64("id").unwrap(), Some(1));
/// assert_eq!(row.get_text("name").unwrap(), Some("Ada"));
/// ```
#[derive(Debug)]
pub struct Response {
    inner: QueryResult,
}

impl Response {
    pub(crate) fn new(inner: QueryResult) -> Self {
        Self { inner }
    }

    /// The output column names, in row order.
    pub fn columns(&self) -> &[String] {
        &self.inner.columns
    }

    /// The position of a named column, if present.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.inner.columns.iter().position(|c| c == name)
    }

    /// The number of rows returned.
    pub fn len(&self) -> usize {
        self.inner.rows.len()
    }

    /// Whether no rows were returned.
    pub fn is_empty(&self) -> bool {
        self.inner.rows.is_empty()
    }

    /// The row at `index`, as a borrowing view.
    pub fn row(&self, index: usize) -> Option<Row<'_>> {
        self.inner.rows.get(index).map(|cells| Row {
            columns: &self.inner.columns,
            cells,
        })
    }

    /// Iterate the rows as borrowing views.
    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> {
        let columns = &self.inner.columns;
        self.inner
            .rows
            .iter()
            .map(move |cells| Row { columns, cells })
    }

    /// The keyset continuation token, present when more pages remain. Feed it
    /// back through a [`crate::Cursor`] to page a stable snapshot.
    pub fn cursor(&self) -> Option<&[u8]> {
        self.inner.cursor.as_deref()
    }

    /// Whether a guarded/conditional write applied (`None` for reads and
    /// unconditional writes).
    pub fn applied(&self) -> Option<bool> {
        self.inner.applied
    }

    /// The number of rows a write changed (`None` for reads).
    pub fn affected(&self) -> Option<u64> {
        self.inner.affected
    }

    /// Consume the response, yielding the underlying wire result.
    pub fn into_inner(self) -> QueryResult {
        self.inner
    }
}

/// A borrowing view of one result row: cells addressable by column name or
/// position, with typed accessors.
#[derive(Clone, Copy)]
pub struct Row<'a> {
    columns: &'a [String],
    cells: &'a [Value],
}

impl<'a> Row<'a> {
    /// The raw cell for a named column, or `None` if the column is unknown.
    pub fn get(&self, column: &str) -> Option<&'a Value> {
        let i = self.columns.iter().position(|c| c == column)?;
        self.cells.get(i)
    }

    /// The raw cell at `index`.
    pub fn at(&self, index: usize) -> Option<&'a Value> {
        self.cells.get(index)
    }

    /// All cells in column order.
    pub fn values(&self) -> &'a [Value] {
        self.cells
    }

    fn cell(&self, column: &str) -> Result<&'a Value, DecodeError> {
        self.get(column)
            .ok_or_else(|| DecodeError::UnknownColumn(column.to_string()))
    }

    /// An `i64` cell: `Ok(None)` for null, `Err` for an unknown column or a
    /// non-integer value.
    pub fn get_i64(&self, column: &str) -> Result<Option<i64>, DecodeError> {
        match self.cell(column)? {
            Value::Null => Ok(None),
            Value::I64(n) => Ok(Some(*n)),
            _ => Err(mismatch(column, "i64")),
        }
    }

    /// An `f64` cell: `Ok(None)` for null, `Err` for an unknown column or a
    /// non-float value.
    pub fn get_f64(&self, column: &str) -> Result<Option<f64>, DecodeError> {
        match self.cell(column)? {
            Value::Null => Ok(None),
            Value::F64(n) => Ok(Some(*n)),
            _ => Err(mismatch(column, "f64")),
        }
    }

    /// A `bool` cell: `Ok(None)` for null, `Err` for an unknown column or a
    /// non-boolean value.
    pub fn get_bool(&self, column: &str) -> Result<Option<bool>, DecodeError> {
        match self.cell(column)? {
            Value::Null => Ok(None),
            Value::Bool(b) => Ok(Some(*b)),
            _ => Err(mismatch(column, "bool")),
        }
    }

    /// A `text` cell: `Ok(None)` for null, `Err` for an unknown column or a
    /// non-text value.
    pub fn get_text(&self, column: &str) -> Result<Option<&'a str>, DecodeError> {
        match self.cell(column)? {
            Value::Null => Ok(None),
            Value::Text(s) => Ok(Some(s.as_str())),
            _ => Err(mismatch(column, "text")),
        }
    }
}

fn mismatch(column: &str, expected: &'static str) -> DecodeError {
    DecodeError::TypeMismatch {
        column: column.to_string(),
        expected,
    }
}
