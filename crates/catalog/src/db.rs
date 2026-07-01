//! The catalog database handle: typed DDL/DML over the transaction layer,
//! and consistent schema+data snapshots.

use std::sync::Arc;

use common::{Clock, IoBackend, Rng};
use txn::{JobDb, TxnError};
use types::{UuidV7Gen, Value};

use crate::codec::decode_table;
use crate::job::{decode_padded, CatalogJob, Env, JobOp, JobOut};
use crate::policy::{RowFilter, RowUpdater};
use crate::schema::{ColumnDef, IndexDef, TableDef};
use crate::store;
use crate::{CatalogCorruption, CatalogError, Result};

/// An embedded relational database: tables, schema, and constraints over the
/// MVCC transaction layer. Cheap to clone; all clones share one writer.
///
/// Generated values (`now`, `uuid_v7`, auto-increment) are driven by the
/// injected [`Clock`] and [`Rng`], so runs are deterministic under simulation.
pub struct Catalog<B: IoBackend> {
    db: JobDb<B, CatalogJob>,
    env: Arc<Env>,
}

impl<B: IoBackend> Clone for Catalog<B> {
    fn clone(&self) -> Self {
        Catalog {
            db: self.db.clone(),
            env: Arc::clone(&self.env),
        }
    }
}

impl<B: IoBackend + 'static> Catalog<B> {
    /// Create a fresh database (an empty catalog) on an empty backend.
    pub fn create(backend: B, clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Result<Self> {
        let env = Arc::new(Env {
            clock: Arc::clone(&clock),
            uuid: UuidV7Gen::new(clock, rng),
        });
        Ok(Catalog {
            db: JobDb::create(backend)?,
            env,
        })
    }

    /// Open an existing database, recovering to its last committed state.
    pub fn open(backend: B, clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Result<Self> {
        let env = Arc::new(Env {
            clock: Arc::clone(&clock),
            uuid: UuidV7Gen::new(clock, rng),
        });
        Ok(Catalog {
            db: JobDb::open(backend)?,
            env,
        })
    }

    fn submit(&self, op: JobOp) -> Result<JobOut> {
        let job = CatalogJob {
            env: Arc::clone(&self.env),
            op,
        };
        let (_txn, out) = self.db.submit(job).map_err(from_txn)?;
        Ok(out)
    }

    /// `create table` — validate and persist a new table definition.
    pub fn create_table(&self, def: TableDef) -> Result<()> {
        self.submit(JobOp::CreateTable(def)).map(|_| ())
    }

    /// `drop table` — remove the definition and free the table's pages.
    pub fn drop_table(&self, table: &str) -> Result<()> {
        self.submit(JobOp::DropTable(table.to_string())).map(|_| ())
    }

    /// `alter table add column`. The new column must be nullable or carry a
    /// constant default (existing rows are padded lazily on read).
    pub fn add_column(&self, table: &str, column: ColumnDef) -> Result<()> {
        self.submit(JobOp::AddColumn {
            table: table.to_string(),
            column,
        })
        .map(|_| ())
    }

    /// `create index` — validate, **backfill from existing rows** (unique
    /// violations reject the DDL), and persist.
    pub fn create_index(&self, table: &str, index: IndexDef) -> Result<()> {
        self.submit(JobOp::CreateIndex {
            table: table.to_string(),
            index,
        })
        .map(|_| ())
    }

    /// `drop index` — remove the definition and free the index's pages. The
    /// implicit index backing a `unique` column cannot be dropped.
    pub fn drop_index(&self, table: &str, index: &str) -> Result<()> {
        self.submit(JobOp::DropIndex {
            table: table.to_string(),
            index: index.to_string(),
        })
        .map(|_| ())
    }

    /// Insert one row (named columns; omitted columns take defaults/generated
    /// values). Returns the full materialized row in schema column order.
    pub fn insert(&self, table: &str, row: Vec<(String, Value)>) -> Result<Vec<Value>> {
        let mut rows = self.insert_many(table, vec![row])?;
        match rows.pop() {
            Some(row) => Ok(row),
            None => Err(internal("insert produced no row")),
        }
    }

    /// Insert several rows **atomically** — one transaction, all or nothing.
    pub fn insert_many(
        &self,
        table: &str,
        rows: Vec<Vec<(String, Value)>>,
    ) -> Result<Vec<Vec<Value>>> {
        match self.submit(JobOp::Insert {
            table: table.to_string(),
            rows,
        })? {
            JobOut::Rows(rows) => Ok(rows),
            _ => Err(internal("unexpected insert output")),
        }
    }

    /// Update the row at `pk` with absolute column sets. Engine-managed
    /// columns (rowversion, `on_update: now`) refresh automatically; PK
    /// columns are immutable. Returns the full updated row.
    pub fn update(
        &self,
        table: &str,
        pk: Vec<Value>,
        sets: Vec<(String, Value)>,
    ) -> Result<Vec<Value>> {
        match self.submit(JobOp::Update {
            table: table.to_string(),
            pk,
            sets,
        })? {
            JobOut::Row(row) => Ok(row),
            _ => Err(internal("unexpected update output")),
        }
    }

    /// Delete the row at `pk`. Returns whether a row was removed.
    pub fn delete(&self, table: &str, pk: Vec<Value>) -> Result<bool> {
        match self.submit(JobOp::Delete {
            table: table.to_string(),
            pk,
        })? {
            JobOut::Deleted(deleted) => Ok(deleted),
            _ => Err(internal("unexpected delete output")),
        }
    }

    /// Conditionally update every row a `policy` matches, evaluating both the
    /// selector and the new values against **live committed rows in the
    /// writer** (`SPEC.md` §6 rule 3). Returns the number of rows changed.
    /// This is the query layer's guarded/relative/optimistic update path.
    pub fn update_where(&self, table: &str, policy: Box<dyn RowUpdater>) -> Result<u64> {
        match self.submit(JobOp::UpdateWhere {
            table: table.to_string(),
            policy,
        })? {
            JobOut::Affected(n) => Ok(n),
            _ => Err(internal("unexpected update_where output")),
        }
    }

    /// Conditionally delete every row a `filter` matches (evaluated against
    /// live rows in the writer). Returns the number of rows removed.
    pub fn delete_where(&self, table: &str, filter: Box<dyn RowFilter>) -> Result<u64> {
        match self.submit(JobOp::DeleteWhere {
            table: table.to_string(),
            filter,
        })? {
            JobOut::Affected(n) => Ok(n),
            _ => Err(internal("unexpected delete_where output")),
        }
    }

    /// Pin a consistent snapshot of schema **and** data.
    pub fn snapshot(&self) -> CatSnapshot<B> {
        CatSnapshot {
            snap: self.db.snapshot(),
        }
    }

    /// The latest committed transaction id.
    pub fn txn_id(&self) -> u64 {
        self.db.txn_id()
    }

    /// Run the pager's integrity check, returning storage statistics.
    pub fn validate(&self) -> Result<pager::PagerStats> {
        Ok(self.db.validate()?)
    }
}

/// A consistent read view over schema and data, pinned at one committed
/// version (see [`txn::Snapshot`]).
pub struct CatSnapshot<B: IoBackend> {
    snap: txn::Snapshot<B>,
}

impl<B: IoBackend> CatSnapshot<B> {
    /// The transaction id this snapshot is pinned at.
    pub fn txn_id(&self) -> u64 {
        self.snap.txn_id()
    }

    /// All table names, ascending.
    pub fn tables(&self) -> Result<Vec<String>> {
        let (lo, hi) = store::tbl_band()?;
        let mut names = Vec::new();
        for (_, bytes) in self.snap.range(Some(&lo), Some(&hi))? {
            names.push(decode_table(&bytes)?.name);
        }
        Ok(names)
    }

    /// The definition of `table`.
    pub fn table(&self, table: &str) -> Result<TableDef> {
        let key = store::tbl_key(table)?;
        match self.snap.get(&key)? {
            Some(bytes) => decode_table(&bytes),
            None => Err(CatalogError::UnknownTable {
                table: table.to_string(),
            }),
        }
    }

    /// Look up one row by primary key, in schema column order.
    pub fn get(&self, table: &str, pk: &[Value]) -> Result<Option<Vec<Value>>> {
        let def = self.table(table)?;
        let key = types::encode_key(pk)?;
        match self.snap.get_in(self.data_root(table)?, &key)? {
            Some(bytes) => Ok(Some(decode_padded(&def, &bytes)?)),
            None => Ok(None),
        }
    }

    /// All rows of `table` in primary-key order, each in schema column order.
    pub fn scan(&self, table: &str) -> Result<Vec<Vec<Value>>> {
        let def = self.table(table)?;
        let root = self.data_root(table)?;
        let mut rows = Vec::new();
        for (_, bytes) in self.snap.scan_in(root)? {
            rows.push(decode_padded(&def, &bytes)?);
        }
        Ok(rows)
    }

    /// Cross-check the whole database: every tree is structurally valid and
    /// **every index matches its base table entry-for-entry** (the Phase 7
    /// exit criterion). Read-only, over this snapshot's pinned version.
    pub fn validate(&self) -> Result<()> {
        if let Some(cat_root) = self.snap.root() {
            self.snap.validate_tree(cat_root)?;
        }
        for table in self.tables()? {
            let def = self.table(&table)?;
            let data_root = self.data_root(&table)?;
            self.snap.validate_tree(data_root)?;
            let base = self.snap.scan_in(data_root)?;

            for idx in &def.indexes {
                let iroot = self.index_root(&table, &idx.name)?;
                self.snap.validate_tree(iroot)?;

                // Brute-force expectation from the base rows.
                let mut expected: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                for (pk_key, bytes) in &base {
                    let row = decode_padded(&def, bytes)?;
                    let cols: Vec<types::Value> = idx
                        .columns
                        .iter()
                        .map(|name| {
                            def.col_index(name)
                                .and_then(|ci| row.get(ci).cloned())
                                .unwrap_or(types::Value::Null)
                        })
                        .collect();
                    if let Some(e) = index::entry(&cols, pk_key, idx.unique)? {
                        expected.push((e.key, e.value));
                    }
                }
                expected.sort();

                let actual = self.snap.scan_in(iroot)?;
                if expected != actual {
                    return Err(CatalogError::IndexOutOfSync {
                        table: table.clone(),
                        index: idx.name.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    fn data_root(&self, table: &str) -> Result<pager::PageId> {
        let key = store::root_key(table)?;
        match self.snap.get(&key)? {
            Some(bytes) => store::decode_root(&bytes),
            None => Err(CatalogError::Corrupt(CatalogCorruption::MissingEntry)),
        }
    }

    fn index_root(&self, table: &str, index: &str) -> Result<pager::PageId> {
        let key = store::iroot_key(table, index)?;
        match self.snap.get(&key)? {
            Some(bytes) => store::decode_root(&bytes),
            None => Err(CatalogError::Corrupt(CatalogCorruption::MissingEntry)),
        }
    }
}

/// Map a transaction-layer error back to the catalog's typed error: a
/// rejection carries the original [`CatalogError`] across the writer thread.
fn from_txn(err: TxnError) -> CatalogError {
    match err {
        TxnError::Rejected(inner) => {
            // Capture the category before erasing the `CategorizedError` bound;
            // a rejection that isn't ours is kept typed under `Policy` so its
            // category flows and a higher layer can downcast it further.
            let category = inner.category();
            let any: Box<dyn std::error::Error + Send + Sync> = inner;
            match any.downcast::<CatalogError>() {
                Ok(ours) => *ours,
                Err(source) => CatalogError::Policy { category, source },
            }
        }
        other => CatalogError::Txn(other),
    }
}

fn internal(reason: &str) -> CatalogError {
    CatalogError::InvalidSchema {
        reason: format!("internal: {reason}"),
    }
}
