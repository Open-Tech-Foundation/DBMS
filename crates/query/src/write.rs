//! The write path (`ARCHITECTURE.md` §3.8, `SPEC.md` §5.5/§6).
//!
//! Turns a validated `insert`/`update`/`delete` into a catalog write. Inserts
//! carry literal values and map straight onto the catalog's atomic
//! `insert_many`. Updates and deletes carry a **selector** and (for updates)
//! **set expressions**, which must be evaluated against live committed rows
//! *inside the writer* so the read → check → write is one indivisible step
//! (`SPEC.md` §6 rule 3). The query layer supplies that logic as a
//! [`catalog::RowUpdater`] / [`catalog::RowFilter`] policy; the catalog runs it
//! in its single writer, which is what serializes the bank scenario and gives
//! first-committer-wins on the optimistic path.
//!
//! The request is assumed **validated** (the §6 safety rules — mandatory
//! selector, no guarded blind set — are enforced by [`crate::validate`]).

use common::IoBackend;
use proto::{Delete, Expr, Insert, Request, Selector, Update};
use types::Value;

use catalog::{Catalog, CatalogError, PolicyError, RowFilter, RowUpdater, TableDef, WriteSpec};

use crate::eval::{eval, eval_predicate, Shape};
use crate::QueryError;

/// The outcome of a write, shaped for the `SPEC.md` §5.6 result fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Whether a conditional write took effect (`null` for a plain insert).
    pub applied: Option<bool>,
    /// The number of rows changed.
    pub affected: u64,
}

/// Execute a validated write request against `cat`.
pub fn execute_write<B: IoBackend + 'static>(
    request: &Request,
    cat: &Catalog<B>,
) -> Result<WriteOutcome, QueryError> {
    match request {
        Request::Insert(insert) => run_insert(insert, cat),
        Request::Update(update) => run_update(update, cat),
        Request::Delete(delete) => run_delete(delete, cat),
        // Atomic multi-op transactions submit as one job; that lands with the
        // rest of the write path.
        Request::Transaction(_) => Err(unsupported("multi-op transactions")),
        Request::Select(_) | Request::Explain(_) => Err(unsupported("reads via the write path")),
    }
}

fn run_insert<B: IoBackend + 'static>(
    insert: &Insert,
    cat: &Catalog<B>,
) -> Result<WriteOutcome, QueryError> {
    let rows = cat.insert_many(&insert.table, insert.rows.clone())?;
    Ok(WriteOutcome {
        applied: None,
        affected: rows.len() as u64,
    })
}

fn run_update<B: IoBackend + 'static>(
    update: &Update,
    cat: &Catalog<B>,
) -> Result<WriteOutcome, QueryError> {
    let policy = UpdatePolicy {
        selector: update.selector.clone(),
        set: update.set.clone(),
    };
    let affected = cat
        .update_where(&update.table, Box::new(policy))
        .map_err(recover)?;
    Ok(WriteOutcome {
        applied: Some(affected > 0),
        affected,
    })
}

fn run_delete<B: IoBackend + 'static>(
    delete: &Delete,
    cat: &Catalog<B>,
) -> Result<WriteOutcome, QueryError> {
    let policy = SelectorPolicy {
        selector: delete.selector.clone(),
    };
    let affected = cat
        .delete_where(&delete.table, Box::new(policy))
        .map_err(recover)?;
    Ok(WriteOutcome {
        applied: Some(affected > 0),
        affected,
    })
}

/// Build the catalog [`WriteSpec`] for one write request (for atomic multi-op
/// transactions). Returns `None` for a non-write request.
pub(crate) fn write_spec(request: &Request) -> Option<WriteSpec> {
    match request {
        Request::Insert(insert) => Some(WriteSpec::Insert {
            table: insert.table.clone(),
            rows: insert.rows.clone(),
        }),
        Request::Update(update) => Some(WriteSpec::Update {
            table: update.table.clone(),
            policy: Box::new(UpdatePolicy {
                selector: update.selector.clone(),
                set: update.set.clone(),
            }),
        }),
        Request::Delete(delete) => Some(WriteSpec::Delete {
            table: delete.table.clone(),
            filter: Box::new(SelectorPolicy {
                selector: delete.selector.clone(),
            }),
        }),
        _ => None,
    }
}

/// Map a batch (`write_batch`) rejection back to a typed query error.
pub(crate) fn recover_batch(err: CatalogError) -> QueryError {
    recover(err)
}

/// Recover a policy rejection back into a typed query error: a `CatalogError::
/// Policy` carries the original evaluator error, which we downcast so callers
/// see an `EvalError` rather than a stringly wrapper.
fn recover(err: CatalogError) -> QueryError {
    if let CatalogError::Policy { category, source } = err {
        match source.downcast::<EvalErrorCarrier>() {
            Ok(carrier) => QueryError::Eval(carrier.0),
            Err(source) => QueryError::Catalog(CatalogError::Policy { category, source }),
        }
    } else {
        QueryError::Catalog(err)
    }
}

fn unsupported(what: &'static str) -> QueryError {
    QueryError::Exec(crate::ExecError::Unsupported { feature: what })
}

/// The shape a single-table write resolves column references against: the
/// table's columns, qualified by the table name (an unqualified reference
/// resolves too, matching the validator).
fn shape_of(def: &TableDef) -> Shape {
    let mut shape = Shape::new();
    for col in &def.columns {
        shape.push(Some(def.name.clone()), col.name.clone());
    }
    shape
}

/// Wrap an `EvalError` as a boxed policy error while keeping it downcastable.
fn boxed(err: crate::EvalError) -> PolicyError {
    Box::new(EvalErrorCarrier(err))
}

/// A newtype so the boxed policy error is both a `CategorizedError` (for the
/// writer) and downcastable back to `EvalError` (for `recover`).
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
struct EvalErrorCarrier(crate::EvalError);

impl common::CategorizedError for EvalErrorCarrier {
    fn category(&self) -> common::ErrorCategory {
        self.0.category()
    }
}

/// Evaluate a selector against a live row.
fn selector_matches(
    selector: &Option<Selector>,
    def: &TableDef,
    row: &[Value],
) -> Result<bool, PolicyError> {
    match selector {
        // A validated write always has a selector; treat a (defensive) missing
        // one as matching nothing rather than everything.
        None => Ok(false),
        Some(Selector::All) => Ok(true),
        Some(Selector::Where(pred)) => eval_predicate(pred, row, &shape_of(def)).map_err(boxed),
    }
}

/// The delete/selector-only policy.
struct SelectorPolicy {
    selector: Option<Selector>,
}

impl RowFilter for SelectorPolicy {
    fn matches(&self, def: &TableDef, row: &[Value]) -> Result<bool, PolicyError> {
        selector_matches(&self.selector, def, row)
    }
}

/// The update policy: a selector plus the set expressions to evaluate.
struct UpdatePolicy {
    selector: Option<Selector>,
    set: Vec<(String, Expr)>,
}

impl RowFilter for UpdatePolicy {
    fn matches(&self, def: &TableDef, row: &[Value]) -> Result<bool, PolicyError> {
        selector_matches(&self.selector, def, row)
    }
}

impl RowUpdater for UpdatePolicy {
    fn new_values(
        &self,
        def: &TableDef,
        row: &[Value],
    ) -> Result<Vec<(String, Value)>, PolicyError> {
        let shape = shape_of(def);
        let mut out = Vec::with_capacity(self.set.len());
        for (name, expr) in &self.set {
            out.push((name.clone(), eval(expr, row, &shape).map_err(boxed)?));
        }
        Ok(out)
    }
}
