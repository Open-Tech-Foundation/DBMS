//! The pull-based (streaming) executor and keyset cursor pagination
//! (`ARCHITECTURE.md` §3.8, `SPEC.md` §5.6).
//!
//! The operators compose as **row iterators**: each pulls from its child on
//! demand, so a `Limit` over an unsorted scan stops early instead of
//! materializing the table. The streamable operators (scan, filter, project,
//! nested-loop join, limit) pull row-at-a-time; the inherently blocking ones
//! (sort, aggregate, distinct) buffer their input and reuse the reference
//! executor's tested computation — the same split a Volcano engine makes. The
//! streaming path is checked for **result-equivalence against the reference
//! executor** (the Phase 9 exit criterion).
//!
//! **Keyset pagination.** A `Limit`/`Cursor` at the top of the plan is peeled
//! off and applied as a resume-then-page pass over the ordered rows: the
//! `cursor` token carries the last row's sort key (order-preserving
//! `encode_row` under the tamper-checked envelope), and a resume returns the
//! rows strictly after it. Within a pinned snapshot no row is skipped or
//! duplicated. (Holding one snapshot *across* pages — full §7.4 stability — is
//! the cursor-owns-its-snapshot public API of Phase 10.) Keyset resume assumes
//! the trailing sort key is unique; append a unique column otherwise.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use common::IoBackend;
use proto::{Expr, JoinKind, Plan, Projection, SortKey};
use types::Value;

use catalog::CatSnapshot;

use crate::eval::{eval, eval_predicate, value_order, Shape};
use crate::exec::{aggregate, distinct, pk_lookup, scan, sort, Relation};
use crate::ExecError;

type Row = Vec<Value>;
type Result<T> = std::result::Result<T, ExecError>;
type RowIter = Box<dyn Iterator<Item = Result<Row>>>;

/// Per-query resource caps (`SPEC.md` §8, `ARCHITECTURE.md` §6). Breaching one
/// is a clean [`ExecError::ResourceLimit`], never an OOM or a hang.
///
/// `max_rows` bounds the rows buffered at any materialization point — a sort,
/// group, distinct, join inner side, or the final page — so an accidental cross
/// join or an unbounded sort fails fast instead of exhausting memory.
#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    /// Maximum rows a single operator may buffer / the query may materialize.
    pub max_rows: u64,
    /// Maximum number of join operators a plan may contain.
    pub max_joins: u32,
    /// Optional wall-clock execution deadline for one query.
    pub deadline: Option<Duration>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            max_rows: 10_000_000,
            max_joins: 16,
            deadline: None,
        }
    }
}

/// A live budget derived from [`ResourceLimits`] for one execution: the row cap
/// plus the resolved deadline instant.
struct Budget {
    max_rows: u64,
    deadline: Option<Instant>,
}

impl Budget {
    fn new(limits: &ResourceLimits) -> Self {
        Budget {
            max_rows: limits.max_rows,
            deadline: limits.deadline.map(|d| Instant::now() + d),
        }
    }

    /// Fail if the deadline has passed (checked periodically, not per row).
    fn check_deadline(&self) -> Result<()> {
        if let Some(end) = self.deadline {
            if Instant::now() >= end {
                return Err(ExecError::ResourceLimit {
                    what: "execution deadline".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// One page of results: the column shape, the page's rows, and a continuation
/// token when more rows remain.
#[derive(Debug)]
pub struct Page {
    /// The output columns.
    pub shape: Shape,
    /// This page's rows.
    pub rows: Vec<Row>,
    /// A keyset continuation token, present when a later page exists.
    pub cursor: Option<Vec<u8>>,
}

/// A streaming operator: its output shape and a pull iterator over its rows.
struct Node {
    shape: Shape,
    iter: RowIter,
}

/// Execute a physical plan and return one page, applying keyset pagination when
/// the plan carries a top-level `Limit`/`Cursor`. Uses the default
/// [`ResourceLimits`]; see [`execute_page_with`] to configure the caps.
pub fn execute_page<B: IoBackend>(plan: &Plan, snap: &CatSnapshot<B>) -> Result<Page> {
    execute_page_with(plan, snap, &ResourceLimits::default())
}

/// Execute a physical plan and return one page under the given resource caps.
pub fn execute_page_with<B: IoBackend>(
    plan: &Plan,
    snap: &CatSnapshot<B>,
    limits: &ResourceLimits,
) -> Result<Page> {
    check_join_count(plan, limits)?;
    let budget = Budget::new(limits);
    // Peel the pagination wrappers. Either nesting order (`Cursor{Limit{..}}`
    // as lowered, or `Limit{Cursor{..}}`) means the same thing: resume, then
    // take a page.
    let mut core = plan;
    let mut limit: Option<u64> = None;
    let mut offset: u64 = 0;
    let mut resume: Option<Vec<u8>> = None;
    loop {
        match core {
            Plan::Limit {
                input,
                limit: l,
                offset: o,
            } => {
                limit = *l;
                offset = *o;
                core = input;
            }
            Plan::Cursor { input, token } => {
                resume = Some(token.clone());
                core = input;
            }
            _ => break,
        }
    }
    // Keyset ordering: the sort keys the pagination is over, if the core is
    // ordered (the canonical paginated shape).
    let sort_keys = match core {
        Plan::Sort { keys, .. } => Some(keys.clone()),
        _ => None,
    };

    let node = build(core, snap, &budget)?;
    let shape = node.shape.clone();
    let mut rows = collect(node.iter, &budget)?;

    // Resume: drop everything up to and including the cursor's position.
    if let Some(token) = resume {
        let key = decode_resume(&token)?;
        let keys = sort_keys.as_ref().ok_or(ExecError::BadCursor)?;
        rows.retain(|row| row_after(row, &key, keys, &shape));
    }

    if offset > 0 {
        let drop = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(rows.len());
        rows.drain(..drop);
    }

    let mut cursor = None;
    if let Some(n) = limit {
        let n = usize::try_from(n).unwrap_or(usize::MAX);
        let more = rows.len() > n;
        rows.truncate(n);
        // A full page with rows behind it earns a continuation token.
        if more {
            if let (Some(keys), Some(last)) = (&sort_keys, rows.last()) {
                cursor = Some(encode_resume(last, keys, &shape)?);
            }
        }
    }

    Ok(Page {
        shape,
        rows,
        cursor,
    })
}

/// Execute a plan and materialize every row (no pagination). Used by the
/// equivalence tests against the reference executor.
pub fn execute_stream<B: IoBackend>(plan: &Plan, snap: &CatSnapshot<B>) -> Result<Relation> {
    let budget = Budget::new(&ResourceLimits::default());
    let node = build(plan, snap, &budget)?;
    drain(node, &budget)
}

/// Build the operator iterator for a plan.
fn build<B: IoBackend>(plan: &Plan, snap: &CatSnapshot<B>, budget: &Budget) -> Result<Node> {
    match plan {
        Plan::Scan { table, alias } => from_relation(scan(snap, table, alias.as_deref(), None)?),
        Plan::IndexScan {
            table,
            alias,
            index,
            prefix,
        } => from_relation(scan(snap, table, alias.as_deref(), Some((index, prefix)))?),
        Plan::PkLookup { table, alias, key } => {
            from_relation(pk_lookup(snap, table, alias.as_deref(), key)?)
        }
        Plan::Filter { input, pred } => {
            let child = build(input, snap, budget)?;
            let shape = child.shape.clone();
            let pred = pred.clone();
            let probe = shape.clone();
            let iter = child.iter.filter_map(move |row| match row {
                Ok(row) => match eval_predicate(&pred, &row, &probe) {
                    Ok(true) => Some(Ok(row)),
                    Ok(false) => None,
                    Err(e) => Some(Err(ExecError::Eval(e))),
                },
                Err(e) => Some(Err(e)),
            });
            Ok(Node {
                shape,
                iter: Box::new(iter),
            })
        }
        Plan::Project { input, items } => project_stream(build(input, snap, budget)?, items),
        Plan::Join {
            kind,
            left,
            right,
            on,
        } => join_stream(
            *kind,
            build(left, snap, budget)?,
            build(right, snap, budget)?,
            on.clone(),
            budget,
        ),
        // Blocking operators: buffer the input, reuse the reference executor's
        // computation, then stream the result.
        Plan::Aggregate { input, by, aggs } => {
            let rel = drain(build(input, snap, budget)?, budget)?;
            from_relation(aggregate(&rel, by, aggs)?)
        }
        Plan::Sort { input, keys } => {
            let rel = drain(build(input, snap, budget)?, budget)?;
            from_relation(sort(rel, keys)?)
        }
        Plan::Distinct { input } => {
            let rel = drain(build(input, snap, budget)?, budget)?;
            from_relation(distinct(rel))
        }
        Plan::Limit {
            input,
            limit,
            offset,
        } => {
            // A non-top (nested) limit streams with an early stop.
            let child = build(input, snap, budget)?;
            let skip = usize::try_from(*offset).unwrap_or(usize::MAX);
            let iter: RowIter = match limit {
                Some(n) => {
                    let take = usize::try_from(*n).unwrap_or(usize::MAX);
                    Box::new(child.iter.skip(skip).take(take))
                }
                None => Box::new(child.iter.skip(skip)),
            };
            Ok(Node {
                shape: child.shape,
                iter,
            })
        }
        // A cursor only appears at the plan top, handled by `execute_page`.
        Plan::Cursor { .. } => Err(ExecError::Unsupported {
            feature: "a cursor below the plan root",
        }),
    }
}

fn project_stream(child: Node, items: &[Projection]) -> Result<Node> {
    let mut shape = Shape::new();
    for (i, item) in items.iter().enumerate() {
        shape.push(None, crate::validate::output_name_of(item, i));
    }
    let items = items.to_vec();
    let input_shape = child.shape;
    let iter = child.iter.map(move |row| {
        let row = row?;
        let mut out = Vec::with_capacity(items.len());
        for item in &items {
            let expr = match item {
                Projection::Aliased { expr, .. } | Projection::Expr(expr) => expr,
            };
            out.push(eval(expr, &row, &input_shape).map_err(ExecError::Eval)?);
        }
        Ok(out)
    });
    Ok(Node {
        shape,
        iter: Box::new(iter),
    })
}

fn join_stream(
    kind: JoinKind,
    left: Node,
    right: Node,
    on: Option<Expr>,
    budget: &Budget,
) -> Result<Node> {
    let shape = left.shape.clone().concat(right.shape.clone());
    let right_width = right.shape.cols.len();
    // The inner side is scanned once per outer row, so it is buffered.
    let right_rows: Vec<Row> = collect(right.iter, budget)?;
    let probe = shape.clone();
    let iter = left.iter.flat_map(move |lrow| {
        let lrow = match lrow {
            Ok(r) => r,
            Err(e) => return vec![Err(e)].into_iter(),
        };
        let mut out = Vec::new();
        let mut matched = false;
        for rrow in &right_rows {
            let mut combined = lrow.clone();
            combined.extend(rrow.iter().cloned());
            let keep = match &on {
                None => Ok(true),
                Some(pred) => eval_predicate(pred, &combined, &probe).map_err(ExecError::Eval),
            };
            match keep {
                Ok(true) => {
                    matched = true;
                    out.push(Ok(combined));
                }
                Ok(false) => {}
                Err(e) => out.push(Err(e)),
            }
        }
        if kind == JoinKind::Left && !matched {
            let mut combined = lrow;
            combined.extend(std::iter::repeat_n(Value::Null, right_width));
            out.push(Ok(combined));
        }
        out.into_iter()
    });
    Ok(Node {
        shape,
        iter: Box::new(iter),
    })
}

fn from_relation(rel: Relation) -> Result<Node> {
    Ok(Node {
        shape: rel.shape,
        iter: Box::new(rel.rows.into_iter().map(Ok)),
    })
}

fn drain(node: Node, budget: &Budget) -> Result<Relation> {
    Ok(Relation {
        shape: node.shape,
        rows: collect(node.iter, budget)?,
    })
}

/// Materialize an operator's rows, enforcing the row cap and the deadline.
/// This is the one choke point every buffering step (sort, group, distinct,
/// join inner side, final page) flows through, so the caps hold everywhere.
fn collect(iter: RowIter, budget: &Budget) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    for (i, item) in iter.enumerate() {
        if out.len() as u64 >= budget.max_rows {
            return Err(ExecError::ResourceLimit {
                what: format!("materialized more than {} rows", budget.max_rows),
            });
        }
        // Poll the deadline periodically rather than on every row.
        if i % 4096 == 0 {
            budget.check_deadline()?;
        }
        out.push(item?);
    }
    Ok(out)
}

/// Reject a plan whose join count exceeds the cap, before executing it.
fn check_join_count(plan: &Plan, limits: &ResourceLimits) -> Result<()> {
    let joins = count_joins(plan);
    if joins > limits.max_joins {
        return Err(ExecError::ResourceLimit {
            what: format!("plan uses {joins} joins (cap is {})", limits.max_joins),
        });
    }
    Ok(())
}

/// The number of join operators anywhere in a plan tree.
fn count_joins(plan: &Plan) -> u32 {
    match plan {
        Plan::Scan { .. } | Plan::IndexScan { .. } | Plan::PkLookup { .. } => 0,
        Plan::Join { left, right, .. } => 1 + count_joins(left) + count_joins(right),
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Distinct { input }
        | Plan::Limit { input, .. }
        | Plan::Cursor { input, .. } => count_joins(input),
    }
}

// --- keyset pagination helpers -----------------------------------------------

/// The sort-key values of `row` under `keys` (evaluated against `shape`).
fn key_of(row: &[Value], keys: &[SortKey], shape: &Shape) -> Result<Vec<Value>> {
    keys.iter()
        .map(|k| eval(&k.expr, row, shape).map_err(ExecError::Eval))
        .collect()
}

/// Compare two sort-key tuples under the keys' directions.
fn key_cmp(a: &[Value], b: &[Value], keys: &[SortKey]) -> Ordering {
    for (i, key) in keys.iter().enumerate() {
        let ord = value_order(&a[i], &b[i]);
        let ord = if key.dir == proto::Dir::Desc {
            ord.reverse()
        } else {
            ord
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Whether `row` sorts strictly after the resume key (so it belongs on the
/// next page).
fn row_after(row: &[Value], resume: &[Value], keys: &[SortKey], shape: &Shape) -> bool {
    match key_of(row, keys, shape) {
        Ok(key) => key_cmp(&key, resume, keys) == Ordering::Greater,
        // A row whose key fails to evaluate cannot be positioned; drop it
        // rather than risk a duplicate.
        Err(_) => false,
    }
}

/// Encode a continuation token from a row's sort key: the key values as an
/// (order-preserving) row under the tamper-checked cursor envelope.
fn encode_resume(row: &[Value], keys: &[SortKey], shape: &Shape) -> Result<Vec<u8>> {
    let key = key_of(row, keys, shape)?;
    let payload = types::encode_row(&key)?;
    Ok(proto::encode_cursor_token(&payload))
}

/// Recover the sort-key values a token carries, rejecting a mangled envelope.
fn decode_resume(token: &[u8]) -> Result<Vec<Value>> {
    let payload = proto::decode_cursor_token(token).map_err(|_| ExecError::BadCursor)?;
    types::decode_row(&payload).map_err(|_| ExecError::BadCursor)
}
