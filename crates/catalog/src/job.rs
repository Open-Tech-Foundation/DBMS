//! The catalog's write transactions: DDL and row DML as one [`WriteJob`],
//! enforcing every `SPEC.md` §4 constraint under the validate-then-apply
//! contract — all probes and checks complete before the first tree mutation,
//! so a rejected transaction is a guaranteed no-op.
//!
//! Secondary indexes are maintained **inside the same job** as the base-row
//! change (`ARCHITECTURE.md` §3.6): one commit, one published root — base and
//! indexes are never observed out of sync.

use std::collections::HashSet;
use std::sync::Arc;

use common::{Clock, IoBackend};
use index::Entry;
use pager::PageId;
use txn::{TxnError, WriteCtx, WriteJob};
use types::{encode_key, encode_row, UuidV7Gen, Value};

use crate::codec::{decode_table, encode_table};
use crate::schema::{ColumnDef, DefaultSpec, IndexDef, TableDef};
use crate::store;
use crate::CatalogError;

/// Shared host services for generated values.
pub(crate) struct Env {
    pub clock: Arc<dyn Clock>,
    pub uuid: UuidV7Gen,
}

/// One catalog write transaction.
pub(crate) struct CatalogJob {
    pub env: Arc<Env>,
    pub op: JobOp,
}

pub(crate) enum JobOp {
    CreateTable(TableDef),
    DropTable(String),
    AddColumn {
        table: String,
        column: ColumnDef,
    },
    CreateIndex {
        table: String,
        index: IndexDef,
    },
    DropIndex {
        table: String,
        index: String,
    },
    Insert {
        table: String,
        rows: Vec<Vec<(String, Value)>>,
    },
    Update {
        table: String,
        pk: Vec<Value>,
        sets: Vec<(String, Value)>,
    },
    Delete {
        table: String,
        pk: Vec<Value>,
    },
    /// A conditional update whose selector and set values are evaluated against
    /// live rows in the writer (the query layer's guarded/relative/optimistic
    /// update path).
    UpdateWhere {
        table: String,
        policy: Box<dyn crate::RowUpdater>,
    },
    /// A conditional delete whose selector is evaluated against live rows.
    DeleteWhere {
        table: String,
        filter: Box<dyn crate::RowFilter>,
    },
    /// An atomic multi-op transaction: a sequence of writes committed as one
    /// writer transaction (all or nothing).
    Batch(Vec<crate::WriteSpec>),
}

/// A catalog job's output, delivered after durable commit.
pub(crate) enum JobOut {
    Unit,
    /// The fully materialized inserted rows (generated values included).
    Rows(Vec<Vec<Value>>),
    /// The updated row.
    Row(Vec<Value>),
    /// Whether the delete removed a row.
    Deleted(bool),
    /// The number of rows changed by a conditional update/delete.
    Affected(u64),
    /// The per-op affected counts of a batch, in order.
    Batch(Vec<u64>),
}

impl<B: IoBackend> WriteJob<B> for CatalogJob {
    type Out = JobOut;

    fn apply(self, ctx: &mut WriteCtx<'_, B>) -> txn::Result<JobOut> {
        match self.op {
            JobOp::CreateTable(def) => create_table(ctx, def),
            JobOp::DropTable(name) => drop_table(ctx, &name),
            JobOp::AddColumn { table, column } => add_column(ctx, &table, column),
            JobOp::CreateIndex { table, index } => create_index(ctx, &table, index),
            JobOp::DropIndex { table, index } => drop_index(ctx, &table, &index),
            JobOp::Insert { table, rows } => insert(&self.env, ctx, &table, rows),
            JobOp::Update { table, pk, sets } => update(&self.env, ctx, &table, &pk, sets),
            JobOp::Delete { table, pk } => delete(ctx, &table, &pk),
            JobOp::UpdateWhere { table, policy } => {
                update_where(&self.env, ctx, &table, policy.as_ref())
            }
            JobOp::DeleteWhere { table, filter } => delete_where(ctx, &table, filter.as_ref()),
            JobOp::Batch(specs) => batch(&self.env, ctx, specs),
        }
    }
}

/// Run a sequence of writes as one atomic transaction: each op sees the
/// previous ops' effects (they thread through the evolving published root), and
/// any failure rejects the whole batch.
fn batch<B: IoBackend>(
    env: &Env,
    ctx: &mut WriteCtx<'_, B>,
    specs: Vec<crate::WriteSpec>,
) -> txn::Result<JobOut> {
    let mut counts = Vec::with_capacity(specs.len());
    for spec in specs {
        let affected = match spec {
            crate::WriteSpec::Insert { table, rows } => match insert(env, ctx, &table, rows)? {
                JobOut::Rows(rows) => rows.len() as u64,
                _ => 0,
            },
            crate::WriteSpec::Update { table, policy } => {
                match update_where(env, ctx, &table, policy.as_ref())? {
                    JobOut::Affected(n) => n,
                    _ => 0,
                }
            }
            crate::WriteSpec::Delete { table, filter } => {
                match delete_where(ctx, &table, filter.as_ref())? {
                    JobOut::Affected(n) => n,
                    _ => 0,
                }
            }
        };
        counts.push(affected);
    }
    Ok(JobOut::Batch(counts))
}

/// Wrap a catalog error as a transaction rejection. (If its category is
/// `Corruption` the writer escalates it to fatal — a damaged catalog must
/// stop the writer, not be retried.)
fn rej(err: CatalogError) -> TxnError {
    TxnError::Rejected(Box::new(err))
}

fn idx_err(err: index::IndexError) -> TxnError {
    match err {
        // Unwrap transaction errors so the writer's fatal classification
        // sees them directly; everything else is a typed rejection.
        index::IndexError::Txn(e) => e,
        other => rej(CatalogError::Index(other)),
    }
}

// --- catalog-tree access ---------------------------------------------------

fn load_table<B: IoBackend>(ctx: &WriteCtx<'_, B>, table: &str) -> txn::Result<TableDef> {
    let key = store::tbl_key(table).map_err(rej)?;
    match ctx.lookup(ctx.root(), &key)? {
        Some(bytes) => decode_table(&bytes).map_err(rej),
        None => Err(rej(CatalogError::UnknownTable {
            table: table.to_string(),
        })),
    }
}

fn load_root<B: IoBackend>(ctx: &WriteCtx<'_, B>, table: &str) -> txn::Result<PageId> {
    let key = store::root_key(table).map_err(rej)?;
    match ctx.lookup(ctx.root(), &key)? {
        Some(bytes) => store::decode_root(&bytes).map_err(rej),
        None => Err(rej(CatalogError::Corrupt(
            crate::CatalogCorruption::MissingEntry,
        ))),
    }
}

fn load_seq<B: IoBackend>(ctx: &WriteCtx<'_, B>, table: &str) -> txn::Result<i64> {
    let key = store::seq_key(table).map_err(rej)?;
    match ctx.lookup(ctx.root(), &key)? {
        Some(bytes) => store::decode_seq(&bytes).map_err(rej),
        None => Ok(1),
    }
}

/// The root of every index of `def`, parallel to `def.indexes`.
fn load_index_roots<B: IoBackend>(
    ctx: &WriteCtx<'_, B>,
    def: &TableDef,
) -> txn::Result<Vec<PageId>> {
    let mut roots = Vec::with_capacity(def.indexes.len());
    for idx in &def.indexes {
        let key = store::iroot_key(&def.name, &idx.name).map_err(rej)?;
        match ctx.lookup(ctx.root(), &key)? {
            Some(bytes) => roots.push(store::decode_root(&bytes).map_err(rej)?),
            None => {
                return Err(rej(CatalogError::Corrupt(
                    crate::CatalogCorruption::MissingEntry,
                )))
            }
        }
    }
    Ok(roots)
}

fn store_entry<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    key: Vec<u8>,
    value: Vec<u8>,
) -> txn::Result<()> {
    let cat = ctx.insert(ctx.root(), &key, &value)?;
    ctx.set_root(cat);
    Ok(())
}

fn store_iroot<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    index: &str,
    root: PageId,
) -> txn::Result<()> {
    store_entry(
        ctx,
        store::iroot_key(table, index).map_err(rej)?,
        store::encode_root(root),
    )
}

// --- index helpers -----------------------------------------------------------

/// The values of `idx`'s columns within a full row, in index order.
fn index_cols(def: &TableDef, idx: &IndexDef, row: &[Value]) -> Vec<Value> {
    idx.columns
        .iter()
        .map(|name| {
            def.col_index(name)
                .and_then(|ci| row.get(ci).cloned())
                .unwrap_or(Value::Null)
        })
        .collect()
}

/// The entry `row` contributes to `idx`, if any.
fn entry_for(
    def: &TableDef,
    idx: &IndexDef,
    row: &[Value],
    pk_key: &[u8],
) -> txn::Result<Option<Entry>> {
    index::entry(&index_cols(def, idx, row), pk_key, idx.unique).map_err(idx_err)
}

/// Compute every entry of a fresh index over the existing rows, rejecting
/// unique violations. Read-only (validation phase of a backfill).
fn compute_backfill<B: IoBackend>(
    ctx: &WriteCtx<'_, B>,
    def: &TableDef,
    data_root: PageId,
    idx: &IndexDef,
) -> txn::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    for (pk_key, bytes) in ctx.scan(data_root, None, None)? {
        let row = decode_padded(def, &bytes).map_err(rej)?;
        if let Some(e) = entry_for(def, idx, &row, &pk_key)? {
            if idx.unique && !seen.insert(e.key.clone()) {
                return Err(rej(unique_violation(&def.name, &idx.name)));
            }
            btree::check_entry(&e.key, &e.value).map_err(TxnError::BTree)?;
            entries.push(e);
        }
    }
    Ok(entries)
}

/// Apply a computed backfill: build the tree and persist its root entry.
fn apply_backfill<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    idx: &IndexDef,
    entries: Vec<Entry>,
) -> txn::Result<()> {
    let mut root = ctx.create_tree()?;
    for e in entries {
        root = index::insert_entry(ctx, root, &e).map_err(idx_err)?;
    }
    store_iroot(ctx, table, &idx.name, root)
}

// --- DDL ---------------------------------------------------------------------

fn create_table<B: IoBackend>(ctx: &mut WriteCtx<'_, B>, mut def: TableDef) -> txn::Result<JobOut> {
    def.materialize_implicit_indexes();
    def.validate().map_err(rej)?;
    let tbl_key = store::tbl_key(&def.name).map_err(rej)?;
    if ctx.lookup(ctx.root(), &tbl_key)?.is_some() {
        return Err(rej(CatalogError::TableExists {
            table: def.name.clone(),
        }));
    }
    let bytes = encode_table(&def).map_err(rej)?;
    btree::check_entry(&tbl_key, &bytes).map_err(TxnError::BTree)?;

    let data_root = ctx.create_tree()?;
    store_entry(ctx, tbl_key, bytes)?;
    store_entry(
        ctx,
        store::root_key(&def.name).map_err(rej)?,
        store::encode_root(data_root),
    )?;
    if def.columns.iter().any(|c| c.auto_increment) {
        store_entry(
            ctx,
            store::seq_key(&def.name).map_err(rej)?,
            store::encode_seq(1),
        )?;
    }
    for idx in &def.indexes {
        let iroot = ctx.create_tree()?;
        store_iroot(ctx, &def.name, &idx.name, iroot)?;
    }
    Ok(JobOut::Unit)
}

fn drop_table<B: IoBackend>(ctx: &mut WriteCtx<'_, B>, table: &str) -> txn::Result<JobOut> {
    // Existence check (typed NotFound rather than a silent no-op).
    let _ = load_table(ctx, table)?;
    let data_root = load_root(ctx, table)?;
    ctx.free_tree(data_root)?;

    // Free and unlink every index tree of the table.
    let (lo, hi) = store::iroot_band(table).map_err(rej)?;
    for (key, bytes) in ctx.scan(ctx.root(), Some(&lo), Some(&hi))? {
        let iroot = store::decode_root(&bytes).map_err(rej)?;
        ctx.free_tree(iroot)?;
        let cat = ctx.delete(ctx.root(), &key)?;
        ctx.set_root(cat);
    }

    for key in [
        store::tbl_key(table).map_err(rej)?,
        store::root_key(table).map_err(rej)?,
        store::seq_key(table).map_err(rej)?,
    ] {
        let cat = ctx.delete(ctx.root(), &key)?;
        ctx.set_root(cat);
    }
    Ok(JobOut::Unit)
}

fn add_column<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    column: ColumnDef,
) -> txn::Result<JobOut> {
    let mut def = load_table(ctx, table)?;
    if def.col_index(&column.name).is_some() {
        return Err(rej(CatalogError::InvalidSchema {
            reason: "a column with that name already exists".to_string(),
        }));
    }
    // Existing rows are padded lazily on read, so the new column must read as
    // something: NULL, or a constant default. Generators cannot backfill.
    let backfillable = column.nullable || matches!(column.default, Some(DefaultSpec::Value(_)));
    if !backfillable {
        return Err(rej(CatalogError::InvalidSchema {
            reason: "an added column must be nullable or carry a constant default".to_string(),
        }));
    }
    def.columns.push(column);
    let known = def.indexes.len();
    def.materialize_implicit_indexes();
    def.validate().map_err(rej)?;

    let bytes = encode_table(&def).map_err(rej)?;
    let tbl_key = store::tbl_key(table).map_err(rej)?;
    btree::check_entry(&tbl_key, &bytes).map_err(TxnError::BTree)?;

    // Backfill any implicit unique index the new column brought (rows padded
    // with a shared non-null default will correctly fail here).
    let data_root = load_root(ctx, table)?;
    let mut backfills = Vec::new();
    for idx in &def.indexes[known..] {
        backfills.push((idx.clone(), compute_backfill(ctx, &def, data_root, idx)?));
    }

    store_entry(ctx, tbl_key, bytes)?;
    for (idx, entries) in backfills {
        apply_backfill(ctx, table, &idx, entries)?;
    }
    Ok(JobOut::Unit)
}

fn create_index<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    index: IndexDef,
) -> txn::Result<JobOut> {
    let mut def = load_table(ctx, table)?;
    if def.index_pos(&index.name).is_some() {
        return Err(rej(CatalogError::IndexExists {
            table: table.to_string(),
            index: index.name.clone(),
        }));
    }
    def.indexes.push(index.clone());
    def.validate().map_err(rej)?;

    let bytes = encode_table(&def).map_err(rej)?;
    let tbl_key = store::tbl_key(table).map_err(rej)?;
    btree::check_entry(&tbl_key, &bytes).map_err(TxnError::BTree)?;

    let data_root = load_root(ctx, table)?;
    let entries = compute_backfill(ctx, &def, data_root, &index)?;

    store_entry(ctx, tbl_key, bytes)?;
    apply_backfill(ctx, table, &index, entries)?;
    Ok(JobOut::Unit)
}

fn drop_index<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    index: &str,
) -> txn::Result<JobOut> {
    let mut def = load_table(ctx, table)?;
    let Some(pos) = def.index_pos(index) else {
        return Err(rej(CatalogError::UnknownIndex {
            table: table.to_string(),
            index: index.to_string(),
        }));
    };
    let backing = def
        .indexes
        .get(pos)
        .is_some_and(|idx| def.backs_unique_column(idx));
    if backing {
        return Err(rej(CatalogError::InvalidSchema {
            reason: "this index enforces a unique column and cannot be dropped".to_string(),
        }));
    }
    def.indexes.remove(pos);

    let iroot_key = store::iroot_key(table, index).map_err(rej)?;
    let Some(root_bytes) = ctx.lookup(ctx.root(), &iroot_key)? else {
        return Err(rej(CatalogError::Corrupt(
            crate::CatalogCorruption::MissingEntry,
        )));
    };
    let iroot = store::decode_root(&root_bytes).map_err(rej)?;

    let bytes = encode_table(&def).map_err(rej)?;
    let tbl_key = store::tbl_key(table).map_err(rej)?;

    ctx.free_tree(iroot)?;
    let cat = ctx.delete(ctx.root(), &iroot_key)?;
    ctx.set_root(cat);
    store_entry(ctx, tbl_key, bytes)?;
    Ok(JobOut::Unit)
}

// --- DML ---------------------------------------------------------------------

fn insert<B: IoBackend>(
    env: &Env,
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    input: Vec<Vec<(String, Value)>>,
) -> txn::Result<JobOut> {
    let def = load_table(ctx, table)?;
    let mut data_root = load_root(ctx, table)?;
    let mut index_roots = load_index_roots(ctx, &def)?;
    let has_auto = def.columns.iter().any(|c| c.auto_increment);
    let mut seq = if has_auto { load_seq(ctx, table)? } else { 0 };
    let seq_before = seq;

    // ---- validation (read-only) ----
    let mut staged: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(input.len());
    let mut staged_entries: Vec<Vec<(usize, Entry)>> = Vec::with_capacity(input.len());
    let mut staged_pks: HashSet<Vec<u8>> = HashSet::new();
    let mut staged_unique: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); def.indexes.len()];
    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(input.len());
    for provided in input {
        let row = build_row(env, &def, provided, &mut seq).map_err(rej)?;
        run_checks(&def, &row).map_err(rej)?;

        let pk_key = pk_key_of(&def, &row).map_err(rej)?;
        if staged_pks.contains(&pk_key) || ctx.lookup(data_root, &pk_key)?.is_some() {
            return Err(rej(CatalogError::DuplicateKey {
                table: table.to_string(),
            }));
        }

        // Index entries: probe unique indexes (committed + staged this batch).
        let mut row_entries = Vec::new();
        for (i, idx) in def.indexes.iter().enumerate() {
            let Some(e) = entry_for(&def, idx, &row, &pk_key)? else {
                continue;
            };
            if idx.unique {
                let committed = ctx.lookup(index_roots[i], &e.key)?.is_some();
                if committed || !staged_unique[i].insert(e.key.clone()) {
                    return Err(rej(unique_violation(table, &idx.name)));
                }
            }
            btree::check_entry(&e.key, &e.value).map_err(TxnError::BTree)?;
            row_entries.push((i, e));
        }

        let bytes = encode_row(&row).map_err(|e| rej(e.into()))?;
        btree::check_entry(&pk_key, &bytes).map_err(TxnError::BTree)?;
        staged_pks.insert(pk_key.clone());
        staged.push((pk_key, bytes));
        staged_entries.push(row_entries);
        out_rows.push(row);
    }

    // ---- apply ----
    for ((key, bytes), entries) in staged.into_iter().zip(staged_entries) {
        data_root = ctx.insert(data_root, &key, &bytes)?;
        for (i, e) in entries {
            index_roots[i] = index::insert_entry(ctx, index_roots[i], &e).map_err(idx_err)?;
        }
    }
    store_entry(
        ctx,
        store::root_key(table).map_err(rej)?,
        store::encode_root(data_root),
    )?;
    for (i, idx) in def.indexes.iter().enumerate() {
        store_iroot(ctx, table, &idx.name, index_roots[i])?;
    }
    if seq != seq_before {
        store_entry(
            ctx,
            store::seq_key(table).map_err(rej)?,
            store::encode_seq(seq),
        )?;
    }
    Ok(JobOut::Rows(out_rows))
}

fn update<B: IoBackend>(
    env: &Env,
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    pk: &[Value],
    sets: Vec<(String, Value)>,
) -> txn::Result<JobOut> {
    let def = load_table(ctx, table)?;
    let mut data_root = load_root(ctx, table)?;
    let mut index_roots = load_index_roots(ctx, &def)?;
    let pk_key = encode_pk(table, &def, pk).map_err(rej)?;

    // ---- validation (read-only) ----
    let Some(bytes) = ctx.lookup(data_root, &pk_key)? else {
        return Err(rej(CatalogError::RowNotFound {
            table: table.to_string(),
        }));
    };
    let old_row = decode_padded(&def, &bytes).map_err(rej)?;
    let row = transform_row(env, table, &def, &old_row, sets)?;
    run_checks(&def, &row).map_err(rej)?;

    // Index deltas: only entries whose key changed move; a changed unique key
    // is probed against the index (our own entry sits at the old key, so any
    // hit is another row's — self-exclusion falls out naturally).
    let mut deltas: Vec<(usize, Option<Entry>, Option<Entry>)> = Vec::new();
    for (i, idx) in def.indexes.iter().enumerate() {
        let old_e = entry_for(&def, idx, &old_row, &pk_key)?;
        let new_e = entry_for(&def, idx, &row, &pk_key)?;
        if old_e.as_ref().map(|e| &e.key) == new_e.as_ref().map(|e| &e.key) {
            continue;
        }
        if let Some(e) = &new_e {
            if idx.unique && ctx.lookup(index_roots[i], &e.key)?.is_some() {
                return Err(rej(unique_violation(table, &idx.name)));
            }
            btree::check_entry(&e.key, &e.value).map_err(TxnError::BTree)?;
        }
        deltas.push((i, old_e, new_e));
    }

    let new_bytes = encode_row(&row).map_err(|e| rej(e.into()))?;
    btree::check_entry(&pk_key, &new_bytes).map_err(TxnError::BTree)?;

    // ---- apply ----
    data_root = ctx.insert(data_root, &pk_key, &new_bytes)?;
    let changed: Vec<usize> = deltas.iter().map(|(i, _, _)| *i).collect();
    for (i, old_e, new_e) in deltas {
        if let Some(e) = old_e {
            index_roots[i] = index::remove_entry(ctx, index_roots[i], &e).map_err(idx_err)?;
        }
        if let Some(e) = new_e {
            index_roots[i] = index::insert_entry(ctx, index_roots[i], &e).map_err(idx_err)?;
        }
    }
    store_entry(
        ctx,
        store::root_key(table).map_err(rej)?,
        store::encode_root(data_root),
    )?;
    for i in changed {
        if let Some(idx) = def.indexes.get(i) {
            store_iroot(ctx, table, &idx.name, index_roots[i])?;
        }
    }
    Ok(JobOut::Row(row))
}

fn delete<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    pk: &[Value],
) -> txn::Result<JobOut> {
    let def = load_table(ctx, table)?;
    let mut data_root = load_root(ctx, table)?;
    let mut index_roots = load_index_roots(ctx, &def)?;
    let pk_key = encode_pk(table, &def, pk).map_err(rej)?;
    let Some(bytes) = ctx.lookup(data_root, &pk_key)? else {
        return Ok(JobOut::Deleted(false));
    };
    let row = decode_padded(&def, &bytes).map_err(rej)?;

    data_root = ctx.delete(data_root, &pk_key)?;
    let mut touched = Vec::new();
    for (i, idx) in def.indexes.iter().enumerate() {
        if let Some(e) = entry_for(&def, idx, &row, &pk_key)? {
            index_roots[i] = index::remove_entry(ctx, index_roots[i], &e).map_err(idx_err)?;
            touched.push(i);
        }
    }
    store_entry(
        ctx,
        store::root_key(table).map_err(rej)?,
        store::encode_root(data_root),
    )?;
    for i in touched {
        if let Some(idx) = def.indexes.get(i) {
            store_iroot(ctx, table, &idx.name, index_roots[i])?;
        }
    }
    Ok(JobOut::Deleted(true))
}

/// Apply an absolute `sets` list to a copy of `old_row`, then run the
/// engine-managed bumps (rowversion +1, `on_update: now`). Rejects writes to
/// primary-key and engine-managed columns and type-checks each set cell. The
/// query validator enforces these statically too; this is the durable
/// last-line check against a raw catalog caller.
fn transform_row(
    env: &Env,
    table: &str,
    def: &TableDef,
    old_row: &[Value],
    sets: Vec<(String, Value)>,
) -> txn::Result<Vec<Value>> {
    let mut row = old_row.to_vec();
    for (name, value) in sets {
        let ci = def.col_index(&name).ok_or_else(|| {
            rej(CatalogError::UnknownColumn {
                table: table.to_string(),
                column: name.clone(),
            })
        })?;
        let Some(col) = def.columns.get(ci) else {
            continue;
        };
        if def.is_pk(ci) {
            return Err(rej(CatalogError::PkImmutable { column: name }));
        }
        if col.rowversion || col.on_update_now {
            return Err(rej(CatalogError::EngineManagedColumn { column: name }));
        }
        check_cell_type(table, def, ci, &value).map_err(rej)?;
        if let Some(slot) = row.get_mut(ci) {
            *slot = value;
        }
    }

    let now = Value::Timestamp(env.clock.now_micros());
    for (ci, col) in def.columns.iter().enumerate() {
        if col.rowversion {
            let next = match row.get(ci) {
                Some(Value::I64(v)) => v.saturating_add(1),
                _ => 1,
            };
            if let Some(slot) = row.get_mut(ci) {
                *slot = Value::I64(next);
            }
        }
        if col.on_update_now {
            if let Some(slot) = row.get_mut(ci) {
                *slot = now.clone();
            }
        }
    }
    Ok(row)
}

/// A conditional multi-row update (`SPEC.md` §6 rule 3): the caller's policy is
/// evaluated against **live committed rows inside the writer**, so the
/// read → condition-check → write is one atomic step no client can split —
/// this is what makes guarded and version-guarded updates safe. Returns the
/// number of rows changed. Validate-then-apply: every matched row's new form,
/// CHECK, and unique-index deltas are computed before the first mutation.
fn update_where<B: IoBackend>(
    env: &Env,
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    policy: &dyn crate::RowUpdater,
) -> txn::Result<JobOut> {
    let def = load_table(ctx, table)?;
    let mut data_root = load_root(ctx, table)?;
    let mut index_roots = load_index_roots(ctx, &def)?;

    // ---- validation (read-only) ----
    // Per matched row: (pk_key, new row bytes, per-index (old,new) deltas).
    type RowDelta = (Vec<u8>, Vec<u8>, Vec<(usize, Option<Entry>, Option<Entry>)>);
    let mut matched: Vec<RowDelta> = Vec::new();
    // Track, per unique index, the old keys leaving and the new keys arriving
    // this batch, so an in-batch swap of a unique value is not a false clash.
    let mut removed: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); def.indexes.len()];
    let mut added: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); def.indexes.len()];

    for (pk_key, bytes) in ctx.scan(data_root, None, None)? {
        let old_row = decode_padded(&def, &bytes).map_err(rej)?;
        if !policy.matches(&def, &old_row).map_err(TxnError::Rejected)? {
            continue;
        }
        let sets = policy
            .new_values(&def, &old_row)
            .map_err(TxnError::Rejected)?;
        let row = transform_row(env, table, &def, &old_row, sets)?;
        run_checks(&def, &row).map_err(rej)?;

        let mut deltas = Vec::new();
        for (i, idx) in def.indexes.iter().enumerate() {
            let old_e = entry_for(&def, idx, &old_row, &pk_key)?;
            let new_e = entry_for(&def, idx, &row, &pk_key)?;
            if old_e.as_ref().map(|e| &e.key) == new_e.as_ref().map(|e| &e.key) {
                continue;
            }
            if let Some(e) = &old_e {
                removed[i].insert(e.key.clone());
            }
            if let Some(e) = &new_e {
                btree::check_entry(&e.key, &e.value).map_err(TxnError::BTree)?;
                if idx.unique && !added[i].insert(e.key.clone()) {
                    // Two rows in this batch would take the same unique key.
                    return Err(rej(unique_violation(table, &idx.name)));
                }
            }
            deltas.push((i, old_e, new_e));
        }

        let new_bytes = encode_row(&row).map_err(|e| rej(e.into()))?;
        btree::check_entry(&pk_key, &new_bytes).map_err(TxnError::BTree)?;
        matched.push((pk_key, new_bytes, deltas));
    }

    // A new unique key that hits a committed entry is a violation *unless* that
    // entry is one this batch is removing (a value moving between rows).
    for (i, idx) in def.indexes.iter().enumerate() {
        if !idx.unique {
            continue;
        }
        for key in &added[i] {
            if ctx.lookup(index_roots[i], key)?.is_some() && !removed[i].contains(key) {
                return Err(rej(unique_violation(table, &idx.name)));
            }
        }
    }

    // ---- apply ----
    let affected = matched.len() as u64;
    let mut touched: HashSet<usize> = HashSet::new();
    for (pk_key, new_bytes, deltas) in matched {
        data_root = ctx.insert(data_root, &pk_key, &new_bytes)?;
        for (i, old_e, new_e) in deltas {
            if let Some(e) = old_e {
                index_roots[i] = index::remove_entry(ctx, index_roots[i], &e).map_err(idx_err)?;
            }
            if let Some(e) = new_e {
                index_roots[i] = index::insert_entry(ctx, index_roots[i], &e).map_err(idx_err)?;
            }
            touched.insert(i);
        }
    }
    if affected > 0 {
        store_entry(
            ctx,
            store::root_key(table).map_err(rej)?,
            store::encode_root(data_root),
        )?;
        for i in touched {
            if let Some(idx) = def.indexes.get(i) {
                store_iroot(ctx, table, &idx.name, index_roots[i])?;
            }
        }
    }
    Ok(JobOut::Affected(affected))
}

/// A conditional multi-row delete: the caller's filter runs against live rows
/// in the writer. Returns the number of rows removed.
fn delete_where<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    filter: &dyn crate::RowFilter,
) -> txn::Result<JobOut> {
    let def = load_table(ctx, table)?;
    let mut data_root = load_root(ctx, table)?;
    let mut index_roots = load_index_roots(ctx, &def)?;

    // ---- validation (read-only): collect the doomed rows and their entries.
    // Per matched row: its PK key and the (index, entry) pairs to remove.
    type Doomed = (Vec<u8>, Vec<(usize, Entry)>);
    let mut matched: Vec<Doomed> = Vec::new();
    for (pk_key, bytes) in ctx.scan(data_root, None, None)? {
        let row = decode_padded(&def, &bytes).map_err(rej)?;
        if !filter.matches(&def, &row).map_err(TxnError::Rejected)? {
            continue;
        }
        let mut entries = Vec::new();
        for (i, idx) in def.indexes.iter().enumerate() {
            if let Some(e) = entry_for(&def, idx, &row, &pk_key)? {
                entries.push((i, e));
            }
        }
        matched.push((pk_key, entries));
    }

    // ---- apply ----
    let affected = matched.len() as u64;
    let mut touched: HashSet<usize> = HashSet::new();
    for (pk_key, entries) in matched {
        data_root = ctx.delete(data_root, &pk_key)?;
        for (i, e) in entries {
            index_roots[i] = index::remove_entry(ctx, index_roots[i], &e).map_err(idx_err)?;
            touched.insert(i);
        }
    }
    if affected > 0 {
        store_entry(
            ctx,
            store::root_key(table).map_err(rej)?,
            store::encode_root(data_root),
        )?;
        for i in touched {
            if let Some(idx) = def.indexes.get(i) {
                store_iroot(ctx, table, &idx.name, index_roots[i])?;
            }
        }
    }
    Ok(JobOut::Affected(affected))
}

// --- row construction & checks ----------------------------------------------

/// Materialize a full row from the provided columns: fill defaults and
/// generated values, then type-check every cell (`SPEC.md` §4.1).
fn build_row(
    env: &Env,
    def: &TableDef,
    provided: Vec<(String, Value)>,
    seq: &mut i64,
) -> crate::Result<Vec<Value>> {
    let mut cells: Vec<Option<Value>> = vec![None; def.columns.len()];
    for (name, value) in provided {
        let Some(ci) = def.col_index(&name) else {
            return Err(CatalogError::UnknownColumn {
                table: def.name.clone(),
                column: name,
            });
        };
        let Some(col) = def.columns.get(ci) else {
            continue;
        };
        if col.rowversion || col.on_update_now {
            return Err(CatalogError::EngineManagedColumn { column: name });
        }
        let Some(slot) = cells.get_mut(ci) else {
            continue;
        };
        if slot.is_some() {
            return Err(CatalogError::DuplicateColumn { column: name });
        }
        *slot = Some(value);
    }

    let mut row = Vec::with_capacity(def.columns.len());
    for (ci, col) in def.columns.iter().enumerate() {
        let cell = match cells.get_mut(ci).and_then(Option::take) {
            Some(value) => {
                if col.auto_increment {
                    // An explicit key advances the sequence past it so future
                    // generated keys cannot collide.
                    if let Value::I64(n) = value {
                        *seq = (*seq).max(n.saturating_add(1));
                    }
                }
                value
            }
            None => {
                if col.auto_increment {
                    let v = Value::I64(*seq);
                    *seq = seq.saturating_add(1);
                    v
                } else if col.rowversion {
                    Value::I64(1)
                } else if col.on_update_now {
                    Value::Timestamp(env.clock.now_micros())
                } else {
                    match &col.default {
                        Some(DefaultSpec::Value(v)) => v.clone(),
                        Some(DefaultSpec::Now) => Value::Timestamp(env.clock.now_micros()),
                        Some(DefaultSpec::UuidV7) => Value::Uuid(env.uuid.next_uuid()),
                        None => Value::Null,
                    }
                }
            }
        };
        check_cell_type(&def.name, def, ci, &cell)?;
        row.push(cell);
    }
    Ok(row)
}

/// NOT NULL + declared-type check (including json well-formedness) for one cell.
fn check_cell_type(table: &str, def: &TableDef, ci: usize, value: &Value) -> crate::Result<()> {
    let Some(col) = def.columns.get(ci) else {
        return Ok(());
    };
    match value.kind() {
        None => {
            if !def.is_nullable(ci) {
                return Err(CatalogError::NotNull {
                    table: table.to_string(),
                    column: col.name.clone(),
                });
            }
        }
        Some(kind) => {
            if kind != col.kind {
                return Err(CatalogError::TypeMismatch {
                    table: table.to_string(),
                    column: col.name.clone(),
                    expected: col.kind,
                    found: kind.as_str().to_string(),
                });
            }
            if let Value::Json(doc) = value {
                types::validate_json(doc)?;
            }
        }
    }
    Ok(())
}

/// Evaluate every CHECK; violated only when one is definitively false (3VL).
fn run_checks(def: &TableDef, row: &[Value]) -> crate::Result<()> {
    for (index, check) in def.checks.iter().enumerate() {
        if check.eval(def, row) == Some(false) {
            return Err(CatalogError::CheckViolation {
                table: def.name.clone(),
                index,
            });
        }
    }
    Ok(())
}

/// The encoded PK of a full row.
fn pk_key_of(def: &TableDef, row: &[Value]) -> crate::Result<Vec<u8>> {
    let mut parts = Vec::with_capacity(def.pk.len());
    for name in &def.pk {
        let Some(ci) = def.col_index(name) else {
            continue; // unreachable: definitions are validated
        };
        parts.push(row.get(ci).cloned().unwrap_or(Value::Null));
    }
    Ok(encode_key(&parts)?)
}

/// Validate caller-supplied PK values (arity + kinds) and encode them.
fn encode_pk(table: &str, def: &TableDef, pk: &[Value]) -> crate::Result<Vec<u8>> {
    if pk.len() != def.pk.len() {
        return Err(CatalogError::InvalidSchema {
            reason: format!(
                "primary key takes {} value(s), got {}",
                def.pk.len(),
                pk.len()
            ),
        });
    }
    for (name, value) in def.pk.iter().zip(pk) {
        if let Some(ci) = def.col_index(name) {
            check_cell_type(table, def, ci, value)?;
        }
    }
    Ok(encode_key(pk)?)
}

fn unique_violation(table: &str, index: &str) -> CatalogError {
    CatalogError::UniqueViolation {
        table: table.to_string(),
        index: index.to_string(),
    }
}

/// Decode a stored row and pad trailing columns added after it was written
/// (constant default if any, else NULL).
pub(crate) fn decode_padded(def: &TableDef, bytes: &[u8]) -> crate::Result<Vec<Value>> {
    let mut row = types::decode_row(bytes)?;
    if row.len() > def.columns.len() {
        return Err(CatalogError::Corrupt(
            crate::CatalogCorruption::RowWiderThanSchema,
        ));
    }
    for col in &def.columns[row.len()..] {
        row.push(match &col.default {
            Some(DefaultSpec::Value(v)) => v.clone(),
            _ => Value::Null,
        });
    }
    Ok(row)
}
