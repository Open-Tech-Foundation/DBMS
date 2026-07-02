//! The **reference executor** (`PLAN.md` Phase 9 strategy).
//!
//! A brute-force, obviously-correct interpreter of the logical-plan [`Plan`]
//! over a pinned [`CatSnapshot`]: it materializes every intermediate relation
//! in full, joins by nested loop, and groups/sorts/dedups in memory. It makes
//! no attempt to be fast — its job is to be *transparently right*, so it can
//! serve as the oracle the pull-based (streaming, index-assisted) executor is
//! checked against, and as the first working end-to-end read path.
//!
//! It assumes a **validated** plan (names resolve, expressions type-check,
//! `SPEC.md` §6 holds): the validator runs first, so execution only reports
//! data-dependent failures (arithmetic overflow, `EvalError`s) and storage
//! errors.

use common::IoBackend;
use proto::{AggFunc, Expr, JoinKind, Plan, Projection};
use types::Value;

use catalog::{CatSnapshot, CatalogError};

use crate::eval::{eval, eval_predicate, Shape};
use crate::validate::output_name;
use crate::EvalError;

/// A fully-materialized intermediate (or final) relation: the column shape and
/// the rows, each a row-shaped `Vec<Value>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The columns, describing how to resolve references into `rows`.
    pub shape: Shape,
    /// The rows, in execution order.
    pub rows: Vec<Vec<Value>>,
}

/// How execution can fail (beyond validation, which has already run).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecError {
    /// A runtime expression-evaluation failure (overflow, div-by-zero, cast).
    #[error(transparent)]
    Eval(#[from] EvalError),
    /// A storage/schema error reading the snapshot.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// A value/encoding error (e.g. encoding a cursor token's keyset).
    #[error(transparent)]
    Type(#[from] types::TypeError),
    /// A cursor token that is malformed, tampered, or used without an order.
    #[error("malformed or unusable cursor token")]
    BadCursor,
    /// A per-query resource cap was exceeded (`SPEC.md` §8): too many
    /// materialized rows, too many joins, or the execution deadline.
    #[error("query resource limit exceeded: {what}")]
    ResourceLimit {
        /// Which cap was hit, described for the caller.
        what: String,
    },
    /// A plan node the executor does not run yet.
    #[error("the executor does not support {feature} yet")]
    Unsupported {
        /// The unsupported feature.
        feature: &'static str,
    },
}

impl common::CategorizedError for ExecError {
    fn category(&self) -> common::ErrorCategory {
        match self {
            ExecError::Eval(e) => e.category(),
            ExecError::Catalog(e) => e.category(),
            ExecError::Type(e) => e.category(),
            ExecError::BadCursor | ExecError::Unsupported { .. } => {
                common::ErrorCategory::Validation
            }
            ExecError::ResourceLimit { .. } => common::ErrorCategory::ResourceLimit,
        }
    }
}

type Result<T> = std::result::Result<T, ExecError>;

/// Execute a validated `plan` against `snap`, returning the full result
/// relation. Brute force: every operator materializes its input.
pub fn execute<B: IoBackend>(plan: &Plan, snap: &CatSnapshot<B>) -> Result<Relation> {
    match plan {
        Plan::Scan { table, alias } => scan(snap, table, alias.as_deref(), None),
        Plan::IndexScan {
            table,
            alias,
            index,
            prefix,
        } => scan(snap, table, alias.as_deref(), Some((index, prefix))),
        Plan::PkLookup { table, alias, key } => pk_lookup(snap, table, alias.as_deref(), key),
        Plan::Filter { input, pred } => {
            let rel = execute(input, snap)?;
            let mut rows = Vec::new();
            for row in rel.rows {
                if eval_predicate(pred, &row, &rel.shape)? {
                    rows.push(row);
                }
            }
            Ok(Relation {
                shape: rel.shape,
                rows,
            })
        }
        Plan::Join {
            kind,
            left,
            right,
            on,
        } => join(snap, *kind, left, right, on.as_ref()),
        Plan::Aggregate { input, by, aggs } => aggregate(&execute(input, snap)?, by, aggs),
        Plan::Project { input, items } => project(&execute(input, snap)?, items),
        Plan::Distinct { input } => Ok(distinct(execute(input, snap)?)),
        Plan::Sort { input, keys } => sort(execute(input, snap)?, keys),
        Plan::Limit {
            input,
            limit,
            offset,
        } => Ok(apply_limit(execute(input, snap)?, *limit, *offset)),
        // Keyset pagination pairs with the cursor-token round-trip and lands
        // with the pull-based executor (acceptance scenario 4).
        Plan::Cursor { .. } => Err(ExecError::Unsupported { feature: "cursor" }),
    }
}

/// A primary-key point lookup — a direct base-tree `get` returning the single
/// row with primary key `key`, or no rows. The real seek behind `Plan::PkLookup`.
pub(crate) fn pk_lookup<B: IoBackend>(
    snap: &CatSnapshot<B>,
    table: &str,
    alias: Option<&str>,
    key: &[Value],
) -> Result<Relation> {
    let def = snap.table(table)?;
    let qualifier = alias.unwrap_or(table);
    let mut shape = Shape::new();
    for col in &def.columns {
        shape.push(Some(qualifier.to_string()), col.name.clone());
    }
    let rows = match snap.get(table, key)? {
        Some(row) => vec![row],
        None => Vec::new(),
    };
    Ok(Relation { shape, rows })
}

/// A base-table scan; when `index` is set, rows are filtered to an equality
/// prefix on the indexed columns — the same rows a real index seek returns.
pub(crate) fn scan<B: IoBackend>(
    snap: &CatSnapshot<B>,
    table: &str,
    alias: Option<&str>,
    index: Option<(&str, &[Value])>,
) -> Result<Relation> {
    let def = snap.table(table)?;
    let qualifier = alias.unwrap_or(table);
    let mut shape = Shape::new();
    for col in &def.columns {
        shape.push(Some(qualifier.to_string()), col.name.clone());
    }

    let mut rows = snap.scan(table)?;
    if let Some((index_name, prefix)) = index {
        if let Some(idx) = def.indexes.iter().find(|i| i.name == index_name) {
            let cols: Vec<usize> = idx
                .columns
                .iter()
                .filter_map(|c| def.col_index(c))
                .collect();
            rows.retain(|row| {
                prefix
                    .iter()
                    .zip(&cols)
                    .all(|(want, &ci)| row.get(ci).is_some_and(|got| got == want))
            });
        }
    }
    Ok(Relation { shape, rows })
}

fn join<B: IoBackend>(
    snap: &CatSnapshot<B>,
    kind: JoinKind,
    left: &Plan,
    right: &Plan,
    on: Option<&Expr>,
) -> Result<Relation> {
    let l = execute(left, snap)?;
    let r = execute(right, snap)?;
    let shape = l.shape.clone().concat(r.shape.clone());
    let right_width = r.shape.cols.len();
    let mut rows = Vec::new();

    for lrow in &l.rows {
        let mut matched = false;
        for rrow in &r.rows {
            let mut combined = lrow.clone();
            combined.extend(rrow.iter().cloned());
            let keep = match on {
                None => true, // cross join
                Some(pred) => eval_predicate(pred, &combined, &shape)?,
            };
            if keep {
                matched = true;
                rows.push(combined);
            }
        }
        // LEFT join: an unmatched left row still emits, padded with nulls.
        if kind == JoinKind::Left && !matched {
            let mut combined = lrow.clone();
            combined.extend(std::iter::repeat_n(Value::Null, right_width));
            rows.push(combined);
        }
    }
    Ok(Relation { shape, rows })
}

/// Group rows and compute aggregates. The output row type is the input columns
/// (pass-through, `DECISIONS.md` D24) followed by the named aggregate outputs;
/// each group's pass-through values come from its first row.
pub(crate) fn aggregate(
    input: &Relation,
    by: &[Expr],
    aggs: &[(String, Expr)],
) -> Result<Relation> {
    let mut shape = input.shape.clone();
    for (name, _) in aggs {
        shape.push(None, name.clone());
    }

    // Bucket rows by their group key, preserving first-appearance order.
    let mut keys: Vec<Vec<Value>> = Vec::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, row) in input.rows.iter().enumerate() {
        let key: Vec<Value> = by
            .iter()
            .map(|e| eval(e, row, &input.shape))
            .collect::<std::result::Result<Vec<Value>, EvalError>>()?;
        match keys.iter().position(|k| *k == key) {
            Some(g) => groups[g].push(i),
            None => {
                keys.push(key);
                groups.push(vec![i]);
            }
        }
    }
    // A global aggregate (no group keys) always yields exactly one row, even
    // over empty input.
    if by.is_empty() && groups.is_empty() {
        groups.push(Vec::new());
    }

    let width = input.shape.cols.len();
    let mut rows = Vec::with_capacity(groups.len());
    for members in &groups {
        // Pass-through columns from the first member (all-null if the group is
        // empty, i.e. a global aggregate over no rows).
        let mut out = match members.first() {
            Some(&first) => input.rows[first].clone(),
            None => vec![Value::Null; width],
        };
        for (_, expr) in aggs {
            out.push(compute_agg(expr, members, input)?);
        }
        rows.push(out);
    }
    Ok(Relation { shape, rows })
}

/// Compute one aggregate over a group's member rows.
fn compute_agg(expr: &Expr, members: &[usize], input: &Relation) -> Result<Value> {
    let Expr::Agg { func, arg } = expr else {
        // The validator guarantees group outputs are aggregate calls.
        return Ok(Value::Null);
    };
    // The non-null argument values across the group, in order.
    let mut vals = Vec::new();
    for &i in members {
        let v = eval(arg, &input.rows[i], &input.shape)?;
        if !matches!(v, Value::Null) {
            vals.push(v);
        }
    }
    Ok(match func {
        AggFunc::Count => Value::I64(vals.len() as i64),
        AggFunc::Min => vals
            .into_iter()
            .min_by(crate::eval::value_order)
            .unwrap_or(Value::Null),
        AggFunc::Max => vals
            .into_iter()
            .max_by(crate::eval::value_order)
            .unwrap_or(Value::Null),
        AggFunc::Sum => sum(&vals)?,
        AggFunc::Avg => avg(&vals),
    })
}

/// SUM: `i64` sums are checked (overflow is a typed error); an `f64` present
/// makes the whole sum `f64`; an empty group is null.
fn sum(vals: &[Value]) -> Result<Value> {
    if vals.is_empty() {
        return Ok(Value::Null);
    }
    if vals.iter().any(|v| matches!(v, Value::F64(_))) {
        let mut acc = 0.0;
        for v in vals {
            acc += as_f64(v);
        }
        return Ok(Value::F64(acc));
    }
    let mut acc: i64 = 0;
    for v in vals {
        if let Value::I64(n) = v {
            acc = acc
                .checked_add(*n)
                .ok_or(EvalError::Overflow { op: "sum" })?;
        }
    }
    Ok(Value::I64(acc))
}

/// AVG is always `f64` (null over an empty group).
fn avg(vals: &[Value]) -> Value {
    if vals.is_empty() {
        return Value::Null;
    }
    let total: f64 = vals.iter().map(as_f64).sum();
    Value::F64(total / vals.len() as f64)
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::I64(n) => *n as f64,
        Value::F64(f) => *f,
        _ => f64::NAN,
    }
}

pub(crate) fn project(input: &Relation, items: &[Projection]) -> Result<Relation> {
    let mut shape = Shape::new();
    for (i, item) in items.iter().enumerate() {
        let name = match item {
            Projection::Aliased { name, .. } => name.clone(),
            Projection::Expr(expr) => output_name(expr, i),
        };
        shape.push(None, name);
    }
    let mut rows = Vec::with_capacity(input.rows.len());
    for row in &input.rows {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let expr = match item {
                Projection::Aliased { expr, .. } | Projection::Expr(expr) => expr,
            };
            out.push(eval(expr, row, &input.shape)?);
        }
        rows.push(out);
    }
    Ok(Relation { shape, rows })
}

pub(crate) fn distinct(input: Relation) -> Relation {
    let mut seen: Vec<Vec<Value>> = Vec::new();
    for row in input.rows {
        if !seen.contains(&row) {
            seen.push(row);
        }
    }
    Relation {
        shape: input.shape,
        rows: seen,
    }
}

pub(crate) fn sort(input: Relation, keys: &[proto::SortKey]) -> Result<Relation> {
    // Pre-evaluate the sort keys once per row, then sort — keeps evaluation
    // (which can fail) out of the comparator.
    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(input.rows.len());
    for row in input.rows {
        let ks: Vec<Value> = keys
            .iter()
            .map(|k| eval(&k.expr, &row, &input.shape))
            .collect::<std::result::Result<Vec<Value>, EvalError>>()?;
        keyed.push((ks, row));
    }
    keyed.sort_by(|(a, _), (b, _)| {
        for (i, key) in keys.iter().enumerate() {
            let ord = crate::eval::value_order(&a[i], &b[i]);
            let ord = if key.dir == proto::Dir::Desc {
                ord.reverse()
            } else {
                ord
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(Relation {
        shape: input.shape,
        rows: keyed.into_iter().map(|(_, row)| row).collect(),
    })
}

fn apply_limit(input: Relation, limit: Option<u64>, offset: u64) -> Relation {
    let offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let mut rows: Vec<Vec<Value>> = input.rows.into_iter().skip(offset).collect();
    if let Some(n) = limit {
        rows.truncate(usize::try_from(n).unwrap_or(usize::MAX));
    }
    Relation {
        shape: input.shape,
        rows,
    }
}
