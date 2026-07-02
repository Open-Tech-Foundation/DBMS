//! The embedded database handle: the small, misuse-resistant public surface.
//!
//! [`Database`] owns a [`Catalog`] (the engine) behind a clean API — open or
//! create a database, run DDL, execute a request, run an atomic transaction,
//! open a snapshot-owning [`Cursor`], and check or inspect the file. Closing is
//! just dropping the handle (and every clone).

use std::sync::Arc;

use common::{Clock, IoBackend, MemoryBackend, Rng, SeededRng, SystemClock};

use catalog::{Catalog, ColumnDef, IndexDef, TableDef};
use proto::{Request, Select};

use crate::cursor::Cursor;
use crate::inspect::{Inspection, IntegrityReport, TableInfo};
use crate::result::Response;
use crate::{Error, Result};

/// An embedded relational database. Cheap to clone — every clone shares one
/// writer and one file — and safe to use from many threads.
///
/// Generic over the storage [`IoBackend`]; the [`Database::create`] /
/// [`Database::open`] constructors give the file-backed form, and
/// [`Database::create_memory`] an in-memory one for tests and examples.
///
/// # Examples
///
/// ```
/// use otf_dbms::{ColumnDef, Database, Insert, Request, TableDef, TypeKind, Value};
///
/// let db = Database::create_memory().unwrap();
/// db.create_table(TableDef::new(
///     "users",
///     vec![
///         ColumnDef::new("id", TypeKind::I64),
///         ColumnDef::new("name", TypeKind::Text).not_null(),
///     ],
///     vec!["id"],
/// ))
/// .unwrap();
///
/// db.execute(&Request::Insert(Insert {
///     table: "users".into(),
///     rows: vec![vec![
///         ("id".into(), Value::I64(1)),
///         ("name".into(), Value::Text("Ada".into())),
///     ]],
/// }))
/// .unwrap();
/// ```
pub struct Database<B: IoBackend> {
    cat: Catalog<B>,
}

impl<B: IoBackend> Clone for Database<B> {
    fn clone(&self) -> Self {
        Database {
            cat: self.cat.clone(),
        }
    }
}

impl Database<MemoryBackend> {
    /// Create a fresh in-memory database. Everything lives in RAM and is gone
    /// when the last handle drops — ideal for tests and examples.
    ///
    /// # Examples
    ///
    /// ```
    /// use otf_dbms::Database;
    ///
    /// let db = Database::create_memory().unwrap();
    /// assert!(db.inspect().unwrap().tables.is_empty());
    /// ```
    pub fn create_memory() -> Result<Self> {
        let (clock, rng) = default_services();
        Self::create_with(MemoryBackend::new(), clock, rng)
    }
}

#[cfg(unix)]
impl Database<common::RealFileBackend> {
    /// Create a fresh database at `path`. The path must be new or empty;
    /// creating over an existing database is rejected (open it instead).
    pub fn create(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let backend = common::RealFileBackend::open(path)?;
        if backend.len()? != 0 {
            return Err(Error::Usage(
                "create: file is not empty; use Database::open to reopen it",
            ));
        }
        let (clock, rng) = default_services();
        Self::create_with(backend, clock, rng)
    }

    /// Open the existing database at `path`, recovering it to its last
    /// committed state.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let backend = common::RealFileBackend::open(path)?;
        let (clock, rng) = default_services();
        Self::open_with(backend, clock, rng)
    }
}

impl<B: IoBackend + 'static> Database<B> {
    /// Create a database over an arbitrary backend with injected time and
    /// randomness (for deterministic simulation).
    pub fn create_with(backend: B, clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Result<Self> {
        Ok(Database {
            cat: Catalog::create(backend, clock, rng)?,
        })
    }

    /// Open a database over an arbitrary backend with injected services.
    pub fn open_with(backend: B, clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Result<Self> {
        Ok(Database {
            cat: Catalog::open(backend, clock, rng)?,
        })
    }

    // --- DDL ------------------------------------------------------------------

    /// `create table`.
    pub fn create_table(&self, def: TableDef) -> Result<()> {
        Ok(self.cat.create_table(def)?)
    }

    /// `drop table`.
    pub fn drop_table(&self, table: &str) -> Result<()> {
        Ok(self.cat.drop_table(table)?)
    }

    /// `alter table add column`.
    pub fn add_column(&self, table: &str, column: ColumnDef) -> Result<()> {
        Ok(self.cat.add_column(table, column)?)
    }

    /// `create index` (backfills from existing rows).
    pub fn create_index(&self, table: &str, index: IndexDef) -> Result<()> {
        Ok(self.cat.create_index(table, index)?)
    }

    /// `drop index`.
    pub fn drop_index(&self, table: &str, index: &str) -> Result<()> {
        Ok(self.cat.drop_index(table, index)?)
    }

    // --- queries & writes -----------------------------------------------------

    /// Validate and run one request — a select/explain, an insert/update/delete,
    /// or a transaction — returning the shaped [`Response`].
    ///
    /// # Examples
    ///
    /// ```
    /// use otf_dbms::{ColumnDef, Database, Insert, Request, Select, Stage, TableDef, TableRef, TypeKind, Value};
    ///
    /// let db = Database::create_memory().unwrap();
    /// db.create_table(TableDef::new(
    ///     "t",
    ///     vec![ColumnDef::new("id", TypeKind::I64)],
    ///     vec!["id"],
    /// ))
    /// .unwrap();
    /// db.execute(&Request::Insert(Insert {
    ///     table: "t".into(),
    ///     rows: vec![vec![("id".into(), Value::I64(42))]],
    /// }))
    /// .unwrap();
    ///
    /// let out = db
    ///     .execute(&Request::Select(Select::Pipeline(vec![Stage::Scan(
    ///         TableRef { table: "t".into(), alias: None },
    ///     )])))
    ///     .unwrap();
    /// assert_eq!(out.row(0).unwrap().get_i64("id").unwrap(), Some(42));
    /// ```
    pub fn execute(&self, request: &Request) -> Result<Response> {
        Ok(Response::new(query::execute_query(request, &self.cat)?))
    }

    /// Run a batch of writes as **one atomic transaction** — all commit
    /// together or none do. A convenience for `execute(&Request::Transaction(..))`.
    pub fn transaction(&self, ops: Vec<Request>) -> Result<Response> {
        self.execute(&Request::Transaction(ops))
    }

    /// The bytes-in/bytes-out form: decode a hardened wire request, run it, and
    /// encode a `SPEC.md` §5.6 result (a failure becomes a typed error result,
    /// never a panic). This is the transport-facing entry point.
    pub fn execute_wire(&self, wire: &[u8]) -> Vec<u8> {
        query::execute_wire(wire, &self.cat)
    }

    /// Open a [`Cursor`] that pins a consistent snapshot and pages an ordered
    /// `select` `page_size` rows at a time. The snapshot is held until the
    /// cursor drops, so a concurrent writer never perturbs the walk.
    pub fn open_cursor(&self, select: Select, page_size: u64) -> Result<Cursor<B>> {
        Cursor::open(self.cat.snapshot(), &select, page_size)
    }

    // --- tools ----------------------------------------------------------------

    /// Run a full integrity check: pager invariants, every B+tree, and each
    /// secondary index verified entry-for-entry against its base table. A
    /// corrupt page surfaces as a `Corruption`-category error.
    ///
    /// # Examples
    ///
    /// ```
    /// use otf_dbms::Database;
    ///
    /// let db = Database::create_memory().unwrap();
    /// let report = db.check().unwrap();
    /// assert_eq!(report.tables_checked, 0);
    /// ```
    pub fn check(&self) -> Result<IntegrityReport> {
        // Pager structure first (meta slots, free-list), then a full tree +
        // index cross-check over a pinned snapshot (which reads every page and
        // so trips any checksum failure).
        let stats = self.cat.validate()?;
        let snap = self.cat.snapshot();
        snap.validate()?;
        let tables_checked = snap.tables()?.len();
        Ok(IntegrityReport {
            stats,
            tables_checked,
        })
    }

    /// Produce a read-only structural summary of the database — storage
    /// statistics and per-table row/index counts (the file-inspector tool).
    pub fn inspect(&self) -> Result<Inspection> {
        let stats = self.cat.validate()?;
        let snap = self.cat.snapshot();
        let mut tables = Vec::new();
        for name in snap.tables()? {
            let def = snap.table(&name)?;
            let rows = snap.row_count(&name)? as usize;
            tables.push(TableInfo {
                name: def.name.clone(),
                columns: def.columns.len(),
                rows,
                indexes: def.indexes.iter().map(|i| i.name.clone()).collect(),
            });
        }
        Ok(Inspection { stats, tables })
    }

    /// The latest committed transaction id.
    pub fn txn_id(&self) -> u64 {
        self.cat.txn_id()
    }

    /// Close the database, releasing this handle. Closing is just dropping;
    /// the file is flushed as writes commit, so there is nothing to flush here.
    /// The database stays alive until the last clone is dropped.
    pub fn close(self) {}
}

/// Real-world defaults: the system clock and a time-seeded PRNG. Determinism is
/// only needed under simulation, which uses [`Database::create_with`].
fn default_services() -> (Arc<dyn Clock>, Arc<dyn Rng>) {
    let clock = SystemClock;
    // Seed the generated-value RNG from the wall clock; uuid_v7's randomness is
    // for uniqueness, not secrecy.
    let seed = clock.now_micros() as u64 ^ 0x9E37_79B9_7F4A_7C15;
    (Arc::new(clock), Arc::new(SeededRng::new(seed)))
}
