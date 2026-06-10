//! The catalog database handle: typed DDL/DML over the transaction layer,
//! and consistent schema+data snapshots.

use std::sync::Arc;

use common::{Clock, IoBackend, Rng};
use txn::{JobDb, TxnError};
use types::{UuidV7Gen, Value};

use crate::codec::decode_table;
use crate::job::{decode_padded, CatalogJob, Env, JobOp, JobOut};
use crate::schema::{ColumnDef, TableDef};
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

    fn data_root(&self, table: &str) -> Result<pager::PageId> {
        let key = store::root_key(table)?;
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
            let any: Box<dyn std::error::Error + Send + Sync> = inner;
            match any.downcast::<CatalogError>() {
                Ok(ours) => *ours,
                Err(other) => internal(&format!("foreign rejection: {other}")),
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
