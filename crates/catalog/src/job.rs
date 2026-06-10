//! The catalog's write transactions: DDL and row DML as one [`WriteJob`],
//! enforcing every `SPEC.md` §4 constraint under the validate-then-apply
//! contract — all probes and checks complete before the first tree mutation,
//! so a rejected transaction is a guaranteed no-op.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use common::{Clock, IoBackend};
use pager::PageId;
use txn::{TxnError, WriteCtx, WriteJob};
use types::{encode_key, encode_row, UuidV7Gen, Value};

use crate::codec::{decode_table, encode_table};
use crate::schema::{ColumnDef, DefaultSpec, TableDef};
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
}

impl<B: IoBackend> WriteJob<B> for CatalogJob {
    type Out = JobOut;

    fn apply(self, ctx: &mut WriteCtx<'_, B>) -> txn::Result<JobOut> {
        match self.op {
            JobOp::CreateTable(def) => create_table(ctx, def),
            JobOp::DropTable(name) => drop_table(ctx, &name),
            JobOp::AddColumn { table, column } => add_column(ctx, &table, column),
            JobOp::Insert { table, rows } => insert(&self.env, ctx, &table, rows),
            JobOp::Update { table, pk, sets } => update(&self.env, ctx, &table, &pk, sets),
            JobOp::Delete { table, pk } => delete(ctx, &table, &pk),
        }
    }
}

/// Wrap a catalog error as a transaction rejection. (If its category is
/// `Corruption` the writer escalates it to fatal — a damaged catalog must
/// stop the writer, not be retried.)
fn rej(err: CatalogError) -> TxnError {
    TxnError::Rejected(Box::new(err))
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

fn store_entry<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    key: Vec<u8>,
    value: Vec<u8>,
) -> txn::Result<()> {
    let cat = ctx.insert(ctx.root(), &key, &value)?;
    ctx.set_root(cat);
    Ok(())
}

// --- DDL ---------------------------------------------------------------------

fn create_table<B: IoBackend>(ctx: &mut WriteCtx<'_, B>, def: TableDef) -> txn::Result<JobOut> {
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
    Ok(JobOut::Unit)
}

fn drop_table<B: IoBackend>(ctx: &mut WriteCtx<'_, B>, table: &str) -> txn::Result<JobOut> {
    // Existence check (typed NotFound rather than a silent no-op).
    let _ = load_table(ctx, table)?;
    let data_root = load_root(ctx, table)?;
    ctx.free_tree(data_root)?;
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
    def.validate().map_err(rej)?;

    let bytes = encode_table(&def).map_err(rej)?;
    let tbl_key = store::tbl_key(table).map_err(rej)?;
    btree::check_entry(&tbl_key, &bytes).map_err(TxnError::BTree)?;
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
    let has_auto = def.columns.iter().any(|c| c.auto_increment);
    let mut seq = if has_auto { load_seq(ctx, table)? } else { 0 };
    let seq_before = seq;

    // ---- validation (read-only) ----
    let unique_cols: Vec<usize> = def
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.unique)
        .map(|(i, _)| i)
        .collect();
    // Provisional scan-based UNIQUE probe (one scan for the whole batch);
    // Phase 7's unique indexes replace it.
    let mut taken: HashMap<usize, HashSet<Vec<u8>>> = HashMap::new();
    if !unique_cols.is_empty() {
        for (_, bytes) in ctx.scan(data_root, None, None)? {
            let row = decode_padded(&def, &bytes).map_err(rej)?;
            for &ci in &unique_cols {
                if let Some(cell) = row.get(ci) {
                    if !matches!(cell, Value::Null) {
                        let enc = encode_cell(cell)?;
                        taken.entry(ci).or_default().insert(enc);
                    }
                }
            }
        }
    }

    let mut staged: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(input.len());
    let mut staged_pks: HashSet<Vec<u8>> = HashSet::new();
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
        for &ci in &unique_cols {
            let Some(cell) = row.get(ci) else { continue };
            if matches!(cell, Value::Null) {
                continue; // NULLs never conflict under UNIQUE.
            }
            let enc = encode_cell(cell)?;
            if !taken.entry(ci).or_default().insert(enc) {
                return Err(rej(unique_violation(table, &def, ci)));
            }
        }

        let bytes = encode_row(&row).map_err(|e| rej(e.into()))?;
        btree::check_entry(&pk_key, &bytes).map_err(TxnError::BTree)?;
        staged_pks.insert(pk_key.clone());
        staged.push((pk_key, bytes));
        out_rows.push(row);
    }

    // ---- apply ----
    for (key, bytes) in staged {
        data_root = ctx.insert(data_root, &key, &bytes)?;
    }
    store_entry(
        ctx,
        store::root_key(table).map_err(rej)?,
        store::encode_root(data_root),
    )?;
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
    let pk_key = encode_pk(table, &def, pk).map_err(rej)?;

    // ---- validation (read-only) ----
    let Some(bytes) = ctx.lookup(data_root, &pk_key)? else {
        return Err(rej(CatalogError::RowNotFound {
            table: table.to_string(),
        }));
    };
    let mut row = decode_padded(&def, &bytes).map_err(rej)?;

    let mut touched_unique: Vec<usize> = Vec::new();
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
        check_cell_type(table, &def, ci, &value).map_err(rej)?;
        if col.unique {
            touched_unique.push(ci);
        }
        if let Some(slot) = row.get_mut(ci) {
            *slot = value;
        }
    }

    // Engine-managed bumps.
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

    run_checks(&def, &row).map_err(rej)?;

    // Provisional scan-based UNIQUE probe over the changed columns, skipping
    // this row itself.
    for &ci in &touched_unique {
        let Some(cell) = row.get(ci) else { continue };
        if matches!(cell, Value::Null) {
            continue;
        }
        let target = encode_cell(cell)?;
        for (other_key, other_bytes) in ctx.scan(data_root, None, None)? {
            if other_key == pk_key {
                continue;
            }
            let other = decode_padded(&def, &other_bytes).map_err(rej)?;
            if let Some(other_cell) = other.get(ci) {
                if !matches!(other_cell, Value::Null) && encode_cell(other_cell)? == target {
                    return Err(rej(unique_violation(table, &def, ci)));
                }
            }
        }
    }

    let new_bytes = encode_row(&row).map_err(|e| rej(e.into()))?;
    btree::check_entry(&pk_key, &new_bytes).map_err(TxnError::BTree)?;

    // ---- apply ----
    data_root = ctx.insert(data_root, &pk_key, &new_bytes)?;
    store_entry(
        ctx,
        store::root_key(table).map_err(rej)?,
        store::encode_root(data_root),
    )?;
    Ok(JobOut::Row(row))
}

fn delete<B: IoBackend>(
    ctx: &mut WriteCtx<'_, B>,
    table: &str,
    pk: &[Value],
) -> txn::Result<JobOut> {
    let def = load_table(ctx, table)?;
    let mut data_root = load_root(ctx, table)?;
    let pk_key = encode_pk(table, &def, pk).map_err(rej)?;
    if ctx.lookup(data_root, &pk_key)?.is_none() {
        return Ok(JobOut::Deleted(false));
    }
    data_root = ctx.delete(data_root, &pk_key)?;
    store_entry(
        ctx,
        store::root_key(table).map_err(rej)?,
        store::encode_root(data_root),
    )?;
    Ok(JobOut::Deleted(true))
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
        let col = &def.columns[ci];
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

/// Encode one cell for unique-probe comparison (order-preserving, canonical —
/// so e.g. all NaNs compare equal, matching the key order).
fn encode_cell(cell: &Value) -> txn::Result<Vec<u8>> {
    encode_key(std::slice::from_ref(cell)).map_err(|e| rej(e.into()))
}

fn unique_violation(table: &str, def: &TableDef, ci: usize) -> CatalogError {
    CatalogError::UniqueViolation {
        table: table.to_string(),
        column: def
            .columns
            .get(ci)
            .map(|c| c.name.clone())
            .unwrap_or_default(),
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
