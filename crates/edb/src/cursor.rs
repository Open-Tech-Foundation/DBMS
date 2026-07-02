//! A keyset cursor that **owns its read snapshot**.
//!
//! A [`Cursor`] pins one consistent snapshot for its whole life and pages over
//! it with continuation tokens. Because the snapshot is held (and only released
//! when the cursor is dropped), a concurrent writer's inserts/updates are
//! invisible to an in-flight walk: no row is skipped or duplicated relative to
//! the cursor's snapshot (acceptance scenario 4 — the cross-page stability that
//! Phase 9 deliberately left to this layer).
//!
//! The paginated query must end in a sort: keyset resume positions the next
//! page by the trailing sort key, so the ordering is what the token encodes.
//! Supply the page size to [`Database::open_cursor`](crate::Database::open_cursor);
//! don't bake a `limit` into the select.

use common::IoBackend;
use proto::{Plan, QueryResult, Select};
use query::QueryError;

use catalog::CatSnapshot;

use crate::result::Response;
use crate::{Error, Result};

/// A forward-only pager over a pinned snapshot. Fetch pages until
/// [`Response::cursor`] is `None` (or [`Cursor::is_done`] is true).
///
/// # Examples
///
/// ```
/// use otf_edb::{ColumnDef, Database, Select, SortKey, Stage, TableRef, TypeKind, Value};
/// use otf_edb::{Dir, Expr, Request};
///
/// let db = Database::create_memory().unwrap();
/// db.create_table(otf_edb::TableDef::new(
///     "n",
///     vec![ColumnDef::new("id", TypeKind::I64)],
///     vec!["id"],
/// ))
/// .unwrap();
/// for id in 1..=7 {
///     db.execute(&Request::Insert(otf_edb::Insert {
///         table: "n".into(),
///         rows: vec![vec![("id".into(), Value::I64(id))]],
///     }))
///     .unwrap();
/// }
///
/// // An ordered select, paged three at a time.
/// let select = Select::Pipeline(vec![
///     Stage::Scan(TableRef { table: "n".into(), alias: None }),
///     Stage::Sort(vec![SortKey {
///         expr: Expr::Column { table: None, column: "id".into() },
///         dir: Dir::Asc,
///     }]),
/// ]);
/// let mut cursor = db.open_cursor(select, 3).unwrap();
///
/// let mut seen = Vec::new();
/// loop {
///     let page = cursor.fetch().unwrap();
///     for row in page.rows() {
///         seen.push(row.get_i64("id").unwrap().unwrap());
///     }
///     if page.cursor().is_none() {
///         break;
///     }
/// }
/// assert_eq!(seen, (1..=7).collect::<Vec<_>>());
/// ```
pub struct Cursor<B: IoBackend> {
    /// The pinned read view; dropping the cursor releases it.
    snap: CatSnapshot<B>,
    /// The planned select, without any pagination wrappers.
    base: Plan,
    /// Rows per page.
    page_size: u64,
    /// The continuation token for the next page, if any.
    token: Option<Vec<u8>>,
    /// The output column labels.
    columns: Vec<String>,
    /// Whether the walk has reached the end.
    done: bool,
}

impl<B: IoBackend> Cursor<B> {
    pub(crate) fn open(snap: CatSnapshot<B>, select: &Select, page_size: u64) -> Result<Self> {
        if page_size == 0 {
            return Err(Error::Usage("cursor page size must be at least 1"));
        }
        // Validate against the snapshot's schema, then plan once.
        query::validate(&proto::Request::Select(select.clone()), &snap)
            .map_err(QueryError::from)?;
        let logical = query::lower(select).map_err(QueryError::from)?;
        let base = query::plan(&logical, &snap).map_err(QueryError::from)?;
        let columns = query::validate_select(select, &snap)
            .map_err(QueryError::from)?
            .into_iter()
            .map(|c| c.name)
            .collect();
        Ok(Cursor {
            snap,
            base,
            page_size,
            token: None,
            columns,
            done: false,
        })
    }

    /// The output column labels of the paged query.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Whether every page has been fetched.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Fetch the next page. When the walk is exhausted this returns an empty
    /// page with no continuation token; further calls keep returning empty.
    pub fn fetch(&mut self) -> Result<Response> {
        if self.done {
            return Ok(Response::new(QueryResult {
                columns: self.columns.clone(),
                ..QueryResult::default()
            }));
        }

        // Wrap the plan with resume-then-page: `Limit { Cursor { base } }`.
        // `execute_page` peels both and reads the sort key from `base`.
        let mut plan = self.base.clone();
        if let Some(token) = &self.token {
            plan = Plan::Cursor {
                input: Box::new(plan),
                token: token.clone(),
            };
        }
        plan = Plan::Limit {
            input: Box::new(plan),
            limit: Some(self.page_size),
            offset: 0,
        };

        let page = query::execute_page(&plan, &self.snap).map_err(QueryError::from)?;
        let columns: Vec<String> = page.shape.cols.iter().map(|c| c.name.clone()).collect();
        match &page.cursor {
            Some(token) => self.token = Some(token.clone()),
            None => self.done = true,
        }
        Ok(Response::new(QueryResult {
            columns,
            rows: page.rows,
            cursor: page.cursor,
            ..QueryResult::default()
        }))
    }

    /// The transaction id this cursor's snapshot is pinned at.
    pub fn txn_id(&self) -> u64 {
        self.snap.txn_id()
    }
}
