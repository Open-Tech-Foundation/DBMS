//! The rule-based planner and EXPLAIN (`ARCHITECTURE.md` §3.8, `SPEC.md` §5.7).
//!
//! The planner rewrites a validated **logical** plan into an equivalent
//! **physical** plan — same [`Plan`] tree, but with access paths and stage
//! order chosen for execution. v1 applies two rules, both semantics-preserving
//! (proven by the "planned == reference" equivalence tests):
//!
//! - **Index selection.** A `Filter` of equality predicates over a `Scan`
//!   becomes an [`Plan::IndexScan`] on the longest secondary-index prefix the
//!   equalities cover, with any leftover predicate kept as a residual filter.
//! - **Filter pushdown.** A `Filter` over an INNER/CROSS `Join` splits into
//!   conjuncts, and each conjunct that references only one side is pushed onto
//!   that side (where it may in turn enable an index) — safe stage reordering.
//!   Pushdown across a LEFT join is skipped (it would change null semantics).
//!
//! Join order stays left-deep (as lowered) and the only physical join is the
//! nested loop, so those choices are structural in v1. EXPLAIN renders the
//! physical plan as an indented operator tree.

use proto::{CmpOp, Expr, Plan, Select};
use types::Value;

use catalog::{CatalogError, TableDef};

use crate::lower::lower;
use crate::validate::validate_select;
use crate::QueryError;

/// Rewrite a validated logical plan into a physical plan.
pub fn plan<S: crate::SchemaView>(logical: &Plan, schema: &S) -> Result<Plan, CatalogError> {
    optimize(logical, schema)
}

/// Lower, validate, plan, and render a select as an EXPLAIN operator tree
/// (`SPEC.md` §5.7) without executing it.
pub fn explain<S: crate::SchemaView>(select: &Select, schema: &S) -> Result<String, QueryError> {
    validate_select(select, schema)?;
    let logical = lower(select)?;
    let physical = plan(&logical, schema)?;
    Ok(render_plan(&physical))
}

fn table_def<S: crate::SchemaView>(schema: &S, table: &str) -> Result<TableDef, CatalogError> {
    schema
        .table(table)?
        .ok_or_else(|| CatalogError::UnknownTable {
            table: table.to_string(),
        })
}

/// Bottom-up rewrite: optimize children, then apply the local rule.
fn optimize<S: crate::SchemaView>(plan: &Plan, schema: &S) -> Result<Plan, CatalogError> {
    Ok(match plan {
        Plan::Scan { .. } | Plan::IndexScan { .. } | Plan::PkLookup { .. } => plan.clone(),
        Plan::Filter { input, pred } => {
            let input = optimize(input, schema)?;
            build_filter(input, pred.clone(), schema)?
        }
        Plan::Join {
            kind,
            left,
            right,
            on,
        } => Plan::Join {
            kind: *kind,
            left: Box::new(optimize(left, schema)?),
            right: Box::new(optimize(right, schema)?),
            on: on.clone(),
        },
        Plan::Aggregate { input, by, aggs } => Plan::Aggregate {
            input: Box::new(optimize(input, schema)?),
            by: by.clone(),
            aggs: aggs.clone(),
        },
        Plan::Project { input, items } => Plan::Project {
            input: Box::new(optimize(input, schema)?),
            items: items.clone(),
        },
        Plan::Distinct { input } => Plan::Distinct {
            input: Box::new(optimize(input, schema)?),
        },
        Plan::Sort { input, keys } => Plan::Sort {
            input: Box::new(optimize(input, schema)?),
            keys: keys.clone(),
        },
        Plan::Limit {
            input,
            limit,
            offset,
        } => Plan::Limit {
            input: Box::new(optimize(input, schema)?),
            limit: *limit,
            offset: *offset,
        },
        Plan::Cursor { input, token } => Plan::Cursor {
            input: Box::new(optimize(input, schema)?),
            token: token.clone(),
        },
    })
}

/// Place a filter over an already-optimized input, applying index selection
/// (over a `Scan`) or pushdown (over an INNER/CROSS `Join`) where possible.
fn build_filter<S: crate::SchemaView>(
    input: Plan,
    pred: Expr,
    schema: &S,
) -> Result<Plan, CatalogError> {
    match input {
        Plan::Scan { table, alias } => index_select(table, alias, pred, schema),
        Plan::Join {
            kind: kind @ (proto::JoinKind::Inner | proto::JoinKind::Cross),
            left,
            right,
            on,
        } => pushdown(kind, *left, *right, on, pred, schema),
        other => Ok(Plan::Filter {
            input: Box::new(other),
            pred,
        }),
    }
}

// --- index selection ---------------------------------------------------------

fn index_select<S: crate::SchemaView>(
    table: String,
    alias: Option<String>,
    pred: Expr,
    schema: &S,
) -> Result<Plan, CatalogError> {
    let def = table_def(schema, &table)?;
    let qualifier = alias.as_deref().unwrap_or(&table);
    let conjuncts = flatten_and(&pred);

    // Equality constraints on this scan's columns: name → (value, conjunct idx).
    let mut eqs: Vec<(String, Value, usize)> = Vec::new();
    for (i, c) in conjuncts.iter().enumerate() {
        if let Some((name, value)) = equality_on(c, qualifier) {
            eqs.push((name, value, i));
        }
    }

    // Best access path first: if the equalities pin *every* primary-key column,
    // the base tree serves it as a single-row point lookup — strictly better
    // than any secondary index (no indirection, at most one row).
    if !def.pk.is_empty() {
        let mut key = Vec::with_capacity(def.pk.len());
        let mut consumed = Vec::new();
        for pk_col in &def.pk {
            let Some((_, value, ci)) = eqs.iter().find(|(n, _, _)| n == pk_col) else {
                key.clear();
                break;
            };
            key.push(value.clone());
            consumed.push(*ci);
        }
        if key.len() == def.pk.len() {
            let residual: Vec<Expr> = conjuncts
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !consumed.contains(i))
                .map(|(_, e)| e)
                .collect();
            return Ok(wrap_filter(Plan::PkLookup { table, alias, key }, residual));
        }
    }

    // The index whose longest leading prefix the equalities fully cover.
    struct Candidate {
        index: String,
        prefix: Vec<Value>,
        consumed: Vec<usize>,
    }
    let mut best: Option<Candidate> = None;
    for idx in &def.indexes {
        let mut prefix = Vec::new();
        let mut consumed = Vec::new();
        for col in &idx.columns {
            let Some((_, value, ci)) = eqs.iter().find(|(n, _, _)| n == col) else {
                break;
            };
            prefix.push(value.clone());
            consumed.push(*ci);
        }
        if !prefix.is_empty() && best.as_ref().is_none_or(|b| prefix.len() > b.prefix.len()) {
            best = Some(Candidate {
                index: idx.name.clone(),
                prefix,
                consumed,
            });
        }
    }

    let Some(Candidate {
        index,
        prefix,
        consumed,
    }) = best
    else {
        // No usable index: keep the plain filtered scan.
        return Ok(Plan::Filter {
            input: Box::new(Plan::Scan { table, alias }),
            pred,
        });
    };

    let scan = Plan::IndexScan {
        table,
        alias,
        index,
        prefix,
    };
    // Residual: the conjuncts the index prefix did not consume.
    let residual: Vec<Expr> = conjuncts
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !consumed.contains(i))
        .map(|(_, e)| e)
        .collect();
    Ok(wrap_filter(scan, residual))
}

/// If `expr` is `col = literal` (either operand order) on a column of `qualifier`,
/// return the (column name, literal value).
fn equality_on(expr: &Expr, qualifier: &str) -> Option<(String, Value)> {
    let Expr::Cmp {
        op: CmpOp::Eq,
        lhs,
        rhs,
    } = expr
    else {
        return None;
    };
    if let (Some(name), Some(v)) = (column_here(lhs, qualifier), as_literal(rhs)) {
        return Some((name, v));
    }
    if let (Some(name), Some(v)) = (column_here(rhs, qualifier), as_literal(lhs)) {
        return Some((name, v));
    }
    None
}

/// The column name if `expr` references a column of `qualifier` (unqualified,
/// or qualified by that name).
fn column_here(expr: &Expr, qualifier: &str) -> Option<String> {
    match expr {
        Expr::Column { table, column } => match table {
            None => Some(column.clone()),
            Some(t) if t == qualifier => Some(column.clone()),
            Some(_) => None,
        },
        _ => None,
    }
}

fn as_literal(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Literal(v) => Some(v.clone()),
        _ => None,
    }
}

// --- filter pushdown ---------------------------------------------------------

fn pushdown<S: crate::SchemaView>(
    kind: proto::JoinKind,
    left: Plan,
    right: Plan,
    on: Option<Expr>,
    pred: Expr,
    schema: &S,
) -> Result<Plan, CatalogError> {
    let left_cols = exposed(&left, schema)?;
    let right_cols = exposed(&right, schema)?;

    let mut push_left = Vec::new();
    let mut push_right = Vec::new();
    let mut keep = Vec::new();
    for c in flatten_and(&pred) {
        let refs = column_refs(&c);
        if refs.iter().all(|r| resolvable(r, &left_cols)) {
            push_left.push(c);
        } else if refs.iter().all(|r| resolvable(r, &right_cols)) {
            push_right.push(c);
        } else {
            keep.push(c);
        }
    }

    let new_left = if push_left.is_empty() {
        left
    } else {
        build_filter(left, and_of(push_left), schema)?
    };
    let new_right = if push_right.is_empty() {
        right
    } else {
        build_filter(right, and_of(push_right), schema)?
    };
    let join = Plan::Join {
        kind,
        left: Box::new(new_left),
        right: Box::new(new_right),
        on,
    };
    Ok(wrap_filter(join, keep))
}

/// The (qualifier, column) pairs a plan exposes — enough to decide which side
/// of a join a predicate references (mirrors the validator's row type).
fn exposed<S: crate::SchemaView>(
    plan: &Plan,
    schema: &S,
) -> Result<Vec<(Option<String>, String)>, CatalogError> {
    Ok(match plan {
        Plan::Scan { table, alias }
        | Plan::IndexScan { table, alias, .. }
        | Plan::PkLookup { table, alias, .. } => {
            let def = table_def(schema, table)?;
            let qualifier = alias.clone().unwrap_or_else(|| table.clone());
            def.columns
                .iter()
                .map(|c| (Some(qualifier.clone()), c.name.clone()))
                .collect()
        }
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Distinct { input }
        | Plan::Limit { input, .. }
        | Plan::Cursor { input, .. } => exposed(input, schema)?,
        Plan::Join { left, right, .. } => {
            let mut cols = exposed(left, schema)?;
            cols.extend(exposed(right, schema)?);
            cols
        }
        Plan::Aggregate { input, aggs, .. } => {
            let mut cols = exposed(input, schema)?;
            cols.extend(aggs.iter().map(|(n, _)| (None, n.clone())));
            cols
        }
        Plan::Project { items, .. } => items
            .iter()
            .enumerate()
            .map(|(i, item)| (None, crate::validate::output_name_of(item, i)))
            .collect(),
    })
}

/// Every column reference in an expression.
fn column_refs(expr: &Expr) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    collect_refs(expr, &mut out);
    out
}

fn collect_refs(expr: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match expr {
        Expr::Column { table, column } => out.push((table.clone(), column.clone())),
        Expr::Literal(_) => {}
        Expr::Cmp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } | Expr::NullIf { lhs, rhs } => {
            collect_refs(lhs, out);
            collect_refs(rhs, out);
        }
        Expr::And(items) | Expr::Or(items) | Expr::Coalesce(items) => {
            items.iter().for_each(|e| collect_refs(e, out));
        }
        Expr::Not(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::Agg { arg: inner, .. }
        | Expr::Cast { expr: inner, .. }
        | Expr::Like { expr: inner, .. } => collect_refs(inner, out),
        Expr::Between { expr, lo, hi } => {
            collect_refs(expr, out);
            collect_refs(lo, out);
            collect_refs(hi, out);
        }
        Expr::InList { expr, list } => {
            collect_refs(expr, out);
            list.iter().for_each(|e| collect_refs(e, out));
        }
    }
}

/// Whether a column reference resolves within a side's exposed columns.
fn resolvable(reference: &(Option<String>, String), cols: &[(Option<String>, String)]) -> bool {
    let (q, name) = reference;
    cols.iter().any(|(cq, cn)| {
        cn == name
            && match q {
                Some(q) => cq.as_deref() == Some(q),
                None => true,
            }
    })
}

// --- expression / predicate helpers ------------------------------------------

/// Flatten a top-level conjunction into its conjuncts (owned).
fn flatten_and(pred: &Expr) -> Vec<Expr> {
    match pred {
        Expr::And(items) => items.iter().flat_map(flatten_and).collect(),
        other => vec![other.clone()],
    }
}

/// Recombine conjuncts into one predicate (assumes non-empty).
fn and_of(mut conjuncts: Vec<Expr>) -> Expr {
    if conjuncts.len() == 1 {
        conjuncts.remove(0)
    } else {
        Expr::And(conjuncts)
    }
}

/// Wrap `input` in a filter of `conjuncts`, or return it bare when there are
/// none left.
fn wrap_filter(input: Plan, conjuncts: Vec<Expr>) -> Plan {
    if conjuncts.is_empty() {
        input
    } else {
        Plan::Filter {
            input: Box::new(input),
            pred: and_of(conjuncts),
        }
    }
}

// --- EXPLAIN rendering -------------------------------------------------------

/// Render a physical plan as an indented operator tree.
pub fn render_plan(plan: &Plan) -> String {
    let mut out = String::new();
    render_node(plan, 0, &mut out);
    out
}

fn render_node(plan: &Plan, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    match plan {
        Plan::Scan { table, alias } => {
            out.push_str(&format!("Scan {table}{}\n", alias_suffix(alias)));
        }
        Plan::IndexScan {
            table,
            alias,
            index,
            prefix,
        } => {
            let vals: Vec<String> = prefix.iter().map(render_value).collect();
            out.push_str(&format!(
                "IndexScan {table}{} using {index} prefix=[{}]\n",
                alias_suffix(alias),
                vals.join(", ")
            ));
        }
        Plan::PkLookup { table, alias, key } => {
            let vals: Vec<String> = key.iter().map(render_value).collect();
            out.push_str(&format!(
                "PkLookup {table}{} key=[{}]\n",
                alias_suffix(alias),
                vals.join(", ")
            ));
        }
        Plan::Filter { input, pred } => {
            out.push_str(&format!("Filter {}\n", render_expr(pred)));
            render_node(input, depth + 1, out);
        }
        Plan::Join {
            kind,
            left,
            right,
            on,
        } => {
            let on = on
                .as_ref()
                .map_or_else(String::new, |e| format!(" on {}", render_expr(e)));
            out.push_str(&format!("Join {}{on}\n", kind.name()));
            render_node(left, depth + 1, out);
            render_node(right, depth + 1, out);
        }
        Plan::Aggregate { input, by, aggs } => {
            let by: Vec<String> = by.iter().map(render_expr).collect();
            let names: Vec<&str> = aggs.iter().map(|(n, _)| n.as_str()).collect();
            out.push_str(&format!(
                "Aggregate by=[{}] aggs=[{}]\n",
                by.join(", "),
                names.join(", ")
            ));
            render_node(input, depth + 1, out);
        }
        Plan::Project { input, items } => {
            let names: Vec<String> = items
                .iter()
                .enumerate()
                .map(|(i, item)| crate::validate::output_name_of(item, i))
                .collect();
            out.push_str(&format!("Project [{}]\n", names.join(", ")));
            render_node(input, depth + 1, out);
        }
        Plan::Distinct { input } => {
            out.push_str("Distinct\n");
            render_node(input, depth + 1, out);
        }
        Plan::Sort { input, keys } => {
            let keys: Vec<String> = keys
                .iter()
                .map(|k| format!("{} {}", render_expr(&k.expr), k.dir.name()))
                .collect();
            out.push_str(&format!("Sort [{}]\n", keys.join(", ")));
            render_node(input, depth + 1, out);
        }
        Plan::Limit {
            input,
            limit,
            offset,
        } => {
            let limit = limit.map_or_else(|| "all".to_string(), |n| n.to_string());
            out.push_str(&format!("Limit {limit} offset {offset}\n"));
            render_node(input, depth + 1, out);
        }
        Plan::Cursor { input, .. } => {
            out.push_str("Cursor\n");
            render_node(input, depth + 1, out);
        }
    }
}

fn alias_suffix(alias: &Option<String>) -> String {
    alias
        .as_ref()
        .map_or_else(String::new, |a| format!(" as {a}"))
}

/// A compact expression rendering for EXPLAIN.
fn render_expr(expr: &Expr) -> String {
    match expr {
        Expr::Column { table, column } => match table {
            Some(t) => format!("{t}.{column}"),
            None => column.clone(),
        },
        Expr::Literal(v) => render_value(v),
        Expr::Cmp { op, lhs, rhs } => {
            format!(
                "{} {} {}",
                render_expr(lhs),
                cmp_symbol(*op),
                render_expr(rhs)
            )
        }
        Expr::Arith { op, lhs, rhs } => {
            format!("{} {} {}", render_expr(lhs), op.name(), render_expr(rhs))
        }
        Expr::And(items) => join_wrapped(items, " and "),
        Expr::Or(items) => join_wrapped(items, " or "),
        Expr::Not(inner) => format!("not {}", render_expr(inner)),
        Expr::IsNull(inner) => format!("{} is null", render_expr(inner)),
        Expr::IsNotNull(inner) => format!("{} is not null", render_expr(inner)),
        Expr::Between { expr, lo, hi } => format!(
            "{} between {} and {}",
            render_expr(expr),
            render_expr(lo),
            render_expr(hi)
        ),
        Expr::InList { expr, list } => {
            let items: Vec<String> = list.iter().map(render_expr).collect();
            format!("{} in [{}]", render_expr(expr), items.join(", "))
        }
        Expr::Like {
            expr,
            pattern,
            case_insensitive,
        } => format!(
            "{} {} {pattern:?}",
            render_expr(expr),
            if *case_insensitive { "ilike" } else { "like" }
        ),
        Expr::Coalesce(items) => format!("coalesce({})", join_plain(items)),
        Expr::NullIf { lhs, rhs } => {
            format!("nullif({}, {})", render_expr(lhs), render_expr(rhs))
        }
        Expr::Cast { expr, to } => format!("cast({} as {})", render_expr(expr), to.as_str()),
        Expr::Agg { func, arg } => format!("{}({})", func.name(), render_expr(arg)),
    }
}

fn join_wrapped(items: &[Expr], sep: &str) -> String {
    let parts: Vec<String> = items.iter().map(render_expr).collect();
    format!("({})", parts.join(sep))
}

fn join_plain(items: &[Expr]) -> String {
    items.iter().map(render_expr).collect::<Vec<_>>().join(", ")
}

fn cmp_symbol(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Lte => "<=",
        CmpOp::Gt => ">",
        CmpOp::Gte => ">=",
    }
}

fn render_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) | Value::Timestamp(n) => n.to_string(),
        Value::F64(f) => f.to_string(),
        Value::Text(s) => format!("{s:?}"),
        Value::Blob(_) => "<blob>".to_string(),
        Value::Uuid(u) => types::uuid_to_string(u),
        Value::Json(_) => "<json>".to_string(),
    }
}
