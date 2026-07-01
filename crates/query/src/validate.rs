//! The validator (`ARCHITECTURE.md` §3.8, `SPEC.md` §6).
//!
//! Standing between lowering and the planner, the validator is the layer that
//! turns a *structurally* well-formed query (grammar checked in `proto`,
//! stage-shape checked in [`crate::lower`]) into one that is **safe to plan and
//! execute** against a concrete schema. It does three things:
//!
//! 1. **Name resolution** — every column reference resolves to exactly one
//!    column of one in-scope table (qualified, unqualified, or ambiguous).
//! 2. **Type-checking** — every expression is well-typed under `SPEC.md` §3
//!    (comparisons compare compatible kinds, arithmetic is numeric, `like` is
//!    over text, predicates are boolean, `json` is opaque, aggregates appear
//!    only where a group defines them).
//! 3. **`SPEC.md` §6 safety rules** — an `update`/`delete` carries a selector;
//!    a `guarded` column never takes a blind absolute set; engine-managed and
//!    primary-key columns are not written.
//!
//! Reads validate over the **IR** ([`Plan`]) — the validator threads a *row
//! type* (the columns visible at each operator) up the tree and returns the
//! query's [`OutputSchema`]. Writes validate over the DML AST directly, since
//! they never lower to a `Plan`.
//!
//! Everything here is `Validation`-category (`SPEC.md` §9): the query is
//! rejected before it touches live state. Live constraint enforcement (NOT
//! NULL, UNIQUE, CHECK, required columns, value typing on insert) stays in the
//! write path, where it runs against committed data and reports `Constraint`.

use common::{CategorizedError, ErrorCategory, IoBackend};
use proto::{
    AggFunc, ArithOp, CmpOp, Delete, Expr, Insert, JoinKind, Plan, Projection, Request, Select,
    Selector, SortKey, Update,
};
use types::{TypeKind, Value};

use catalog::{CatSnapshot, CatalogError, ColumnDef, TableDef, UpdatePolicy};

use crate::lower::{lower, LowerError};

// --- schema source -----------------------------------------------------------

/// A read-only view of the schema the validator resolves names against.
///
/// Abstracted so the validator is independent of storage: a live
/// [`CatSnapshot`] is the production source, and tests can back it with a
/// plain map. `Ok(None)` means "no such table"; `Err` is corruption/IO from
/// the source itself.
pub trait SchemaView {
    /// The definition of `table`, or `None` if it does not exist.
    fn table(&self, table: &str) -> std::result::Result<Option<TableDef>, CatalogError>;
}

impl<B: IoBackend> SchemaView for CatSnapshot<B> {
    fn table(&self, table: &str) -> std::result::Result<Option<TableDef>, CatalogError> {
        match CatSnapshot::table(self, table) {
            Ok(def) => Ok(Some(def)),
            Err(CatalogError::UnknownTable { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

// --- errors ------------------------------------------------------------------

/// How a request fails validation. Every variant is `Validation`-category
/// except the transparent [`ValidateError::Schema`], which carries the source's
/// own category (corruption/IO reaching for a table definition).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValidateError {
    /// The select did not lower into the IR.
    #[error(transparent)]
    Lower(#[from] LowerError),
    /// The schema source failed (corruption or IO).
    #[error(transparent)]
    Schema(#[from] CatalogError),
    /// A referenced table is not in the schema.
    #[error("unknown table {table:?}")]
    UnknownTable {
        /// The missing table name.
        table: String,
    },
    /// A column reference resolves to no in-scope column.
    #[error("unknown column {column:?}{}", qualifier_suffix(.qualifier))]
    UnknownColumn {
        /// The qualifier used, if the reference was qualified.
        qualifier: Option<String>,
        /// The column name.
        column: String,
    },
    /// An unqualified column reference matches more than one in-scope column.
    #[error("column {column:?} is ambiguous across the query's tables")]
    AmbiguousColumn {
        /// The ambiguous column name.
        column: String,
    },
    /// A comparison between incompatible kinds (e.g. `text` vs `i64`).
    #[error("cannot compare {left} with {right}")]
    IncompatibleComparison {
        /// The left operand's rendered kind.
        left: String,
        /// The right operand's rendered kind.
        right: String,
    },
    /// A predicate (WHERE / HAVING / join `on`) is not boolean.
    #[error("the {context} predicate must be boolean, found {found}")]
    NonBooleanPredicate {
        /// Where the predicate appeared.
        context: &'static str,
        /// The rendered kind found.
        found: String,
    },
    /// An arithmetic operand is not numeric.
    #[error("arithmetic requires numeric operands, found {found}")]
    NonNumericArith {
        /// The rendered kind found.
        found: String,
    },
    /// A `like`/`ilike` operand is not text.
    #[error("like requires a text operand, found {found}")]
    NonTextLike {
        /// The rendered kind found.
        found: String,
    },
    /// A `json` value reached a context that requires an ordered/comparable
    /// kind (comparison, `between`, `in`, `order by`, `group by`). `json` is
    /// opaque in v1 (`SPEC.md` §3).
    #[error("json is opaque and cannot be used in a {context}")]
    JsonNotOrderable {
        /// The offending context.
        context: &'static str,
    },
    /// A `cast` to or from `json` (opaque in v1).
    #[error("cannot cast to or from json")]
    JsonCast,
    /// The two branches of `coalesce`/`nullif` have incompatible kinds.
    #[error("incompatible kinds {left} and {right}")]
    IncompatibleKinds {
        /// The first rendered kind.
        left: String,
        /// The conflicting rendered kind.
        right: String,
    },
    /// An aggregate expression appeared outside a `group` stage's aggregate
    /// position (in a filter, projection, sort key, join `on`, or nested in
    /// another aggregate).
    #[error("aggregate expressions are only valid as named group outputs")]
    AggregateNotAllowed,
    /// An `update`/`delete` with neither a `where` selector nor `{all:true}`
    /// (`SPEC.md` §6 rule 1).
    #[error("update/delete requires a where selector or an explicit all:true")]
    MissingSelector,
    /// A blind absolute set to a `guarded` column (`SPEC.md` §6 rule 2): it
    /// needs a relative expression or a guard/version condition, and never
    /// rides on `unconditional:true` (that path is for `free` columns only).
    #[error("guarded column {column:?} cannot take a blind absolute set")]
    GuardedBlindSet {
        /// The guarded column.
        column: String,
    },
    /// A write to an engine-managed column (`rowversion` / `on_update: now`).
    #[error("column {column:?} is engine-managed and cannot be written")]
    EngineManagedWrite {
        /// The engine-managed column.
        column: String,
    },
    /// An update to a primary-key column (immutable in v1, `DECISIONS.md` D17).
    #[error("primary-key column {column:?} cannot be updated")]
    PkImmutable {
        /// The primary-key column.
        column: String,
    },
    /// A `set` expression's type is incompatible with its column.
    #[error("column {column:?} is {expected}, cannot assign {found}")]
    AssignTypeMismatch {
        /// The assigned column.
        column: String,
        /// The column's declared kind.
        expected: TypeKind,
        /// The rendered kind of the expression.
        found: String,
    },
}

impl CategorizedError for ValidateError {
    fn category(&self) -> ErrorCategory {
        match self {
            ValidateError::Lower(e) => e.category(),
            ValidateError::Schema(e) => e.category(),
            // Everything else is a malformed/unsafe query (SPEC §9).
            _ => ErrorCategory::Validation,
        }
    }
}

type Result<T> = std::result::Result<T, ValidateError>;

fn qualifier_suffix(qualifier: &Option<String>) -> String {
    match qualifier {
        Some(q) => format!(" (qualified {q:?})"),
        None => String::new(),
    }
}

// --- the result of validation ------------------------------------------------

/// One column of a query's output: its label, kind, and nullability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputColumn {
    /// The output label (`SPEC.md` §5.6 `columns` entry).
    pub name: String,
    /// The column's kind, or `None` for a pure `null` literal.
    pub kind: Option<TypeKind>,
    /// Whether the column may be null.
    pub nullable: bool,
}

/// The ordered output schema of a read query.
pub type OutputSchema = Vec<OutputColumn>;

/// What a validated request is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validated {
    /// A read (`select`/`explain`) and its output schema.
    Read(OutputSchema),
    /// A write (`insert`/`update`/`delete`/`transaction`).
    Write,
}

/// Validate one request against `schema`.
///
/// Reads lower to the IR and validate over it, returning [`Validated::Read`]
/// with the output schema; writes validate the DML AST and the `SPEC.md` §6
/// safety rules, returning [`Validated::Write`].
pub fn validate<S: SchemaView>(request: &Request, schema: &S) -> Result<Validated> {
    match request {
        Request::Select(select) | Request::Explain(select) => {
            Ok(Validated::Read(validate_select(select, schema)?))
        }
        Request::Insert(insert) => {
            validate_insert(insert, schema)?;
            Ok(Validated::Write)
        }
        Request::Update(update) => {
            validate_update(update, schema)?;
            Ok(Validated::Write)
        }
        Request::Delete(delete) => {
            validate_delete(delete, schema)?;
            Ok(Validated::Write)
        }
        Request::Transaction(ops) => {
            for op in ops {
                validate(op, schema)?;
            }
            Ok(Validated::Write)
        }
    }
}

/// Lower a select and validate the resulting plan, returning its output schema.
pub fn validate_select<S: SchemaView>(select: &Select, schema: &S) -> Result<OutputSchema> {
    let plan = lower(select)?;
    let row = check_plan(&plan, schema)?;
    Ok(row.into_output())
}

// --- the type of an expression -----------------------------------------------

/// The static type of an expression: its kind (`None` = the untyped `null`
/// literal) and whether it may evaluate to null.
#[derive(Debug, Clone, Copy)]
struct FieldType {
    kind: Option<TypeKind>,
    nullable: bool,
}

impl FieldType {
    const NULL: FieldType = FieldType {
        kind: None,
        nullable: true,
    };

    fn of(kind: TypeKind, nullable: bool) -> FieldType {
        FieldType {
            kind: Some(kind),
            nullable,
        }
    }

    /// A non-null boolean (the shape of `is_null`, comparisons over non-null
    /// operands, etc.); callers widen `nullable` as needed.
    fn boolean(nullable: bool) -> FieldType {
        FieldType::of(TypeKind::Bool, nullable)
    }
}

fn is_numeric(kind: TypeKind) -> bool {
    matches!(kind, TypeKind::I64 | TypeKind::F64)
}

// --- the row type flowing through a plan -------------------------------------

/// One column visible at some point in the plan: an optional qualifier (table
/// alias or name), the column name, and its type.
#[derive(Debug, Clone)]
struct Slot {
    qualifier: Option<String>,
    name: String,
    ty: FieldType,
}

/// The set of columns visible at a plan node (its "row type").
#[derive(Debug, Clone, Default)]
struct RowType {
    slots: Vec<Slot>,
}

impl RowType {
    /// The columns of a base table, qualified by `qualifier` (its alias or,
    /// absent one, its name).
    fn of_table(def: &TableDef, qualifier: &str) -> RowType {
        let slots = def
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| Slot {
                qualifier: Some(qualifier.to_string()),
                name: col.name.clone(),
                ty: FieldType::of(col.kind, def.is_nullable(i)),
            })
            .collect();
        RowType { slots }
    }

    /// Resolve a column reference to its type, or a name-resolution error.
    fn resolve(&self, qualifier: Option<&str>, column: &str) -> Result<FieldType> {
        let mut hit: Option<FieldType> = None;
        for slot in &self.slots {
            let name_ok = slot.name == column;
            let qual_ok = match qualifier {
                Some(q) => slot.qualifier.as_deref() == Some(q),
                None => true,
            };
            if name_ok && qual_ok {
                if hit.is_some() && qualifier.is_none() {
                    return Err(ValidateError::AmbiguousColumn {
                        column: column.to_string(),
                    });
                }
                hit = Some(slot.ty);
            }
        }
        hit.ok_or_else(|| ValidateError::UnknownColumn {
            qualifier: qualifier.map(str::to_string),
            column: column.to_string(),
        })
    }

    /// Every column made nullable (the right side of a LEFT join).
    fn made_nullable(mut self) -> RowType {
        for slot in &mut self.slots {
            slot.ty.nullable = true;
        }
        self
    }

    fn concat(mut self, other: RowType) -> RowType {
        self.slots.extend(other.slots);
        self
    }

    fn into_output(self) -> OutputSchema {
        self.slots
            .into_iter()
            .map(|s| OutputColumn {
                name: s.name,
                kind: s.ty.kind,
                nullable: s.ty.nullable,
            })
            .collect()
    }
}

// --- read-plan validation ----------------------------------------------------

/// Validate a plan and return its output row type.
fn check_plan<S: SchemaView>(plan: &Plan, schema: &S) -> Result<RowType> {
    match plan {
        // Lowering never emits `IndexScan`; the planner does, and it has the
        // same output shape as a full scan.
        Plan::Scan { table, alias } | Plan::IndexScan { table, alias, .. } => {
            let def = lookup(schema, table)?;
            let qualifier = alias.as_deref().unwrap_or(table);
            Ok(RowType::of_table(&def, qualifier))
        }
        Plan::Filter { input, pred } => {
            let row = check_plan(input, schema)?;
            check_predicate(pred, &row, "where")?;
            Ok(row)
        }
        Plan::Join {
            kind,
            left,
            right,
            on,
        } => {
            let left = check_plan(left, schema)?;
            let mut right = check_plan(right, schema)?;
            if *kind == JoinKind::Left {
                right = right.made_nullable();
            }
            let combined = left.concat(right);
            if let Some(on) = on {
                check_predicate(on, &combined, "join on")?;
            }
            Ok(combined)
        }
        Plan::Aggregate { input, by, aggs } => {
            let row = check_plan(input, schema)?;
            // Group keys are ordinary expressions, but a group key must be
            // orderable, so `json` keys are rejected.
            for key in by {
                let ty = check_expr(key, &row)?;
                if ty.kind == Some(TypeKind::Json) {
                    return Err(ValidateError::JsonNotOrderable {
                        context: "group by",
                    });
                }
            }
            // Each aggregate output extends the row type with a named column.
            // Base columns stay visible (`DECISIONS.md` D24), so a query may
            // project or filter on them alongside the aggregates.
            let mut out = row.clone();
            for (name, expr) in aggs {
                let ty = check_aggregate(expr, &row)?;
                out.slots.push(Slot {
                    qualifier: None,
                    name: name.clone(),
                    ty,
                });
            }
            Ok(out)
        }
        Plan::Project { input, items } => {
            let row = check_plan(input, schema)?;
            let mut out = RowType::default();
            for (i, item) in items.iter().enumerate() {
                let (name, expr) = match item {
                    Projection::Aliased { name, expr } => (name.clone(), expr),
                    Projection::Expr(expr) => (output_name(expr, i), expr),
                };
                let ty = check_expr(expr, &row)?;
                out.slots.push(Slot {
                    qualifier: None,
                    name,
                    ty,
                });
            }
            Ok(out)
        }
        Plan::Distinct { input } | Plan::Limit { input, .. } | Plan::Cursor { input, .. } => {
            check_plan(input, schema)
        }
        Plan::Sort { input, keys } => {
            let row = check_plan(input, schema)?;
            check_sort_keys(keys, &row)?;
            Ok(row)
        }
    }
}

/// The output label of an unaliased projection item: the referenced column's
/// name, or a positional `col{n}` label for a computed expression.
fn output_name(expr: &Expr, index: usize) -> String {
    match expr {
        Expr::Column { column, .. } => column.clone(),
        _ => format!("col{}", index + 1),
    }
}

fn check_sort_keys(keys: &[SortKey], row: &RowType) -> Result<()> {
    for key in keys {
        let ty = check_expr(&key.expr, row)?;
        if ty.kind == Some(TypeKind::Json) {
            return Err(ValidateError::JsonNotOrderable {
                context: "order by",
            });
        }
    }
    Ok(())
}

fn check_predicate(expr: &Expr, row: &RowType, context: &'static str) -> Result<()> {
    let ty = check_expr(expr, row)?;
    match ty.kind {
        // A bare `null` literal is a (constantly unknown) predicate.
        Some(TypeKind::Bool) | None => Ok(()),
        Some(other) => Err(ValidateError::NonBooleanPredicate {
            context,
            found: other.to_string(),
        }),
    }
}

// --- expression type-checking ------------------------------------------------

/// Type-check an ordinary (non-aggregate) expression against `row`.
fn check_expr(expr: &Expr, row: &RowType) -> Result<FieldType> {
    match expr {
        Expr::Column { table, column } => row.resolve(table.as_deref(), column),
        Expr::Literal(value) => Ok(literal_type(value)),
        Expr::Cmp { op, lhs, rhs } => check_cmp(*op, lhs, rhs, row),
        Expr::And(items) | Expr::Or(items) => {
            let mut nullable = false;
            for item in items {
                let ty = check_expr(item, row)?;
                match ty.kind {
                    Some(TypeKind::Bool) | None => nullable |= ty.nullable,
                    Some(other) => {
                        return Err(ValidateError::NonBooleanPredicate {
                            context: "boolean",
                            found: other.to_string(),
                        })
                    }
                }
            }
            Ok(FieldType::boolean(nullable))
        }
        Expr::Not(inner) => {
            let ty = check_expr(inner, row)?;
            match ty.kind {
                Some(TypeKind::Bool) | None => Ok(FieldType::boolean(ty.nullable)),
                Some(other) => Err(ValidateError::NonBooleanPredicate {
                    context: "boolean",
                    found: other.to_string(),
                }),
            }
        }
        Expr::Arith { op, lhs, rhs } => check_arith(*op, lhs, rhs, row),
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            // Any operand; the result is a defined boolean (never null).
            check_expr(inner, row)?;
            Ok(FieldType::boolean(false))
        }
        Expr::Between { expr, lo, hi } => {
            let e = check_expr(expr, row)?;
            let l = check_expr(lo, row)?;
            let h = check_expr(hi, row)?;
            compatible_cmp(e.kind, l.kind)?;
            compatible_cmp(e.kind, h.kind)?;
            Ok(FieldType::boolean(e.nullable || l.nullable || h.nullable))
        }
        Expr::InList { expr, list } => {
            let e = check_expr(expr, row)?;
            let mut nullable = e.nullable;
            for item in list {
                let t = check_expr(item, row)?;
                compatible_cmp(e.kind, t.kind)?;
                nullable |= t.nullable;
            }
            Ok(FieldType::boolean(nullable))
        }
        Expr::Like { expr, .. } => {
            let e = check_expr(expr, row)?;
            match e.kind {
                Some(TypeKind::Text) | None => Ok(FieldType::boolean(e.nullable)),
                Some(other) => Err(ValidateError::NonTextLike {
                    found: other.to_string(),
                }),
            }
        }
        Expr::Coalesce(items) => {
            // The common kind of the branches; null only if every branch is.
            let mut kind: Option<TypeKind> = None;
            let mut nullable = true;
            for item in items {
                let t = check_expr(item, row)?;
                kind = unify(kind, t.kind)?;
                nullable &= t.nullable;
            }
            Ok(FieldType { kind, nullable })
        }
        Expr::NullIf { lhs, rhs } => {
            let l = check_expr(lhs, row)?;
            let r = check_expr(rhs, row)?;
            compatible_cmp(l.kind, r.kind)?;
            // May null out when the operands are equal.
            Ok(FieldType {
                kind: l.kind,
                nullable: true,
            })
        }
        Expr::Cast { expr, to } => {
            let e = check_expr(expr, row)?;
            if *to == TypeKind::Json || e.kind == Some(TypeKind::Json) {
                return Err(ValidateError::JsonCast);
            }
            Ok(FieldType {
                kind: Some(*to),
                nullable: e.nullable,
            })
        }
        // Aggregates are valid only as named group outputs (checked there).
        Expr::Agg { .. } => Err(ValidateError::AggregateNotAllowed),
    }
}

/// The static type of a scalar literal. `Json`/`Uuid`/`Timestamp` never reach
/// expression position (the grammar only admits scalar literals), but are
/// mapped for completeness.
fn literal_type(value: &Value) -> FieldType {
    match value.kind() {
        None => FieldType::NULL,
        Some(kind) => FieldType::of(kind, false),
    }
}

fn check_cmp(op: CmpOp, lhs: &Expr, rhs: &Expr, row: &RowType) -> Result<FieldType> {
    let _ = op; // every comparison operator has identical typing.
    let l = check_expr(lhs, row)?;
    let r = check_expr(rhs, row)?;
    compatible_cmp(l.kind, r.kind)?;
    Ok(FieldType::boolean(l.nullable || r.nullable))
}

/// Are two operand kinds comparable? Equal kinds compare; the two numeric
/// kinds compare with each other; a `null` literal compares with anything;
/// `json` compares with nothing (opaque, `SPEC.md` §3).
fn compatible_cmp(a: Option<TypeKind>, b: Option<TypeKind>) -> Result<()> {
    match (a, b) {
        (None, _) | (_, None) => Ok(()),
        (Some(x), Some(y)) => {
            if x == TypeKind::Json || y == TypeKind::Json {
                return Err(ValidateError::JsonNotOrderable {
                    context: "comparison",
                });
            }
            if x == y || (is_numeric(x) && is_numeric(y)) {
                Ok(())
            } else {
                Err(ValidateError::IncompatibleComparison {
                    left: x.to_string(),
                    right: y.to_string(),
                })
            }
        }
    }
}

fn check_arith(op: ArithOp, lhs: &Expr, rhs: &Expr, row: &RowType) -> Result<FieldType> {
    let _ = op; // every arithmetic operator has identical typing.
    let l = check_expr(lhs, row)?;
    let r = check_expr(rhs, row)?;
    let lk = numeric_operand(l.kind)?;
    let rk = numeric_operand(r.kind)?;
    // f64 is contagious; two i64s stay i64; a null operand leaves the kind to
    // the other side (or null if both are null).
    let kind = match (lk, rk) {
        (Some(TypeKind::F64), _) | (_, Some(TypeKind::F64)) => Some(TypeKind::F64),
        (Some(TypeKind::I64), _) | (_, Some(TypeKind::I64)) => Some(TypeKind::I64),
        _ => None,
    };
    Ok(FieldType {
        kind,
        nullable: l.nullable || r.nullable,
    })
}

/// A numeric operand's kind, or an error for a non-numeric one. `null` passes
/// (its kind is unknown).
fn numeric_operand(kind: Option<TypeKind>) -> Result<Option<TypeKind>> {
    match kind {
        None => Ok(None),
        Some(k) if is_numeric(k) => Ok(Some(k)),
        Some(other) => Err(ValidateError::NonNumericArith {
            found: other.to_string(),
        }),
    }
}

/// The common kind of two branches (`coalesce`), treating `null` as wildcard.
fn unify(acc: Option<TypeKind>, next: Option<TypeKind>) -> Result<Option<TypeKind>> {
    match (acc, next) {
        (None, k) | (k, None) => Ok(k),
        (Some(a), Some(b)) if a == b => Ok(Some(a)),
        (Some(a), Some(b)) if is_numeric(a) && is_numeric(b) => Ok(Some(TypeKind::F64)),
        (Some(a), Some(b)) => Err(ValidateError::IncompatibleKinds {
            left: a.to_string(),
            right: b.to_string(),
        }),
    }
}

/// Type-check a `group` aggregate output. The expression must be an aggregate
/// call whose argument is an ordinary (non-aggregate) expression.
fn check_aggregate(expr: &Expr, row: &RowType) -> Result<FieldType> {
    let Expr::Agg { func, arg } = expr else {
        // Lowering already rejects non-aggregate group outputs; this guards
        // the invariant for any direct caller.
        return Err(ValidateError::AggregateNotAllowed);
    };
    let arg_ty = check_expr(arg, row)?;
    Ok(match func {
        // COUNT is always a defined i64 (`count(x)` counts non-null args,
        // `count(1)` counts rows; either way, never null).
        AggFunc::Count => FieldType::of(TypeKind::I64, false),
        // SUM/MIN/MAX carry the argument's kind; an empty group yields null.
        AggFunc::Sum | AggFunc::Min | AggFunc::Max => FieldType {
            kind: arg_ty.kind,
            nullable: true,
        },
        // AVG is always f64.
        AggFunc::Avg => FieldType::of(TypeKind::F64, true),
    })
}

// --- write validation --------------------------------------------------------

fn lookup<S: SchemaView>(schema: &S, table: &str) -> Result<TableDef> {
    schema
        .table(table)?
        .ok_or_else(|| ValidateError::UnknownTable {
            table: table.to_string(),
        })
}

fn column<'a>(def: &'a TableDef, name: &str) -> Result<&'a ColumnDef> {
    def.col_index(name)
        .and_then(|i| def.columns.get(i))
        .ok_or_else(|| ValidateError::UnknownColumn {
            qualifier: Some(def.name.clone()),
            column: name.to_string(),
        })
}

fn validate_insert<S: SchemaView>(insert: &Insert, schema: &S) -> Result<()> {
    let def = lookup(schema, &insert.table)?;
    for row in &insert.rows {
        for (name, _value) in row {
            let col = column(&def, name)?;
            reject_engine_managed(col)?;
        }
    }
    Ok(())
}

fn validate_delete<S: SchemaView>(delete: &Delete, schema: &S) -> Result<()> {
    let def = lookup(schema, &delete.table)?;
    let Some(selector) = &delete.selector else {
        return Err(ValidateError::MissingSelector);
    };
    if let Selector::Where(pred) = selector {
        let row = RowType::of_table(&def, &def.name);
        check_predicate(pred, &row, "where")?;
    }
    Ok(())
}

fn validate_update<S: SchemaView>(update: &Update, schema: &S) -> Result<()> {
    let def = lookup(schema, &update.table)?;
    // §6 rule 1: a selector is mandatory.
    let Some(selector) = &update.selector else {
        return Err(ValidateError::MissingSelector);
    };
    let row = RowType::of_table(&def, &def.name);
    if let Selector::Where(pred) = selector {
        check_predicate(pred, &row, "where")?;
    }

    // §6 rule 2: does the selector carry a reliable guard/version condition?
    let guarded_by_where = match selector {
        Selector::Where(pred) => has_guard_predicate(pred, &def),
        Selector::All => false,
    };

    for (name, expr) in &update.set {
        let col = column(&def, name)?;
        if def.pk.contains(name) {
            return Err(ValidateError::PkImmutable {
                column: name.clone(),
            });
        }
        reject_engine_managed(col)?;
        let ty = check_expr(expr, &row)?;
        assignable(name, col, ty)?;

        if col.update == UpdatePolicy::Guarded {
            // A relative expression (one that reads the row) is always allowed;
            // otherwise the set is absolute and needs a guard/version
            // condition. `unconditional:true` never rescues a guarded column.
            let relative = references_column(expr);
            if update.unconditional || (!relative && !guarded_by_where) {
                return Err(ValidateError::GuardedBlindSet {
                    column: name.clone(),
                });
            }
        }
    }
    Ok(())
}

fn reject_engine_managed(col: &ColumnDef) -> Result<()> {
    if col.rowversion || col.on_update_now {
        return Err(ValidateError::EngineManagedWrite {
            column: col.name.clone(),
        });
    }
    Ok(())
}

/// Whether an expression's kind may be assigned to `col`. A `null` literal is
/// left to the write path's NOT NULL check; a known kind must match the column
/// (i64 narrows into i64 only — `f64` into an `i64` column is a mismatch).
fn assignable(name: &str, col: &ColumnDef, ty: FieldType) -> Result<()> {
    match ty.kind {
        None => Ok(()),
        Some(kind) if kind == col.kind => Ok(()),
        Some(other) => Err(ValidateError::AssignTypeMismatch {
            column: name.to_string(),
            expected: col.kind,
            found: other.to_string(),
        }),
    }
}

/// Does the expression read any column (making it a *relative* update)?
fn references_column(expr: &Expr) -> bool {
    match expr {
        Expr::Column { .. } => true,
        Expr::Literal(_) => false,
        Expr::Cmp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } | Expr::NullIf { lhs, rhs } => {
            references_column(lhs) || references_column(rhs)
        }
        Expr::And(items) | Expr::Or(items) | Expr::Coalesce(items) => {
            items.iter().any(references_column)
        }
        Expr::Not(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::Agg { arg: inner, .. }
        | Expr::Cast { expr: inner, .. }
        | Expr::Like { expr: inner, .. } => references_column(inner),
        Expr::Between { expr, lo, hi } => {
            references_column(expr) || references_column(lo) || references_column(hi)
        }
        Expr::InList { expr, list } => {
            references_column(expr) || list.iter().any(references_column)
        }
    }
}

/// Does the `where` carry a comparison that guards against a lost update — a
/// predicate on a `guarded` or `rowversion` column?
///
/// The scan is deliberately conservative: it credits guards under a top-level
/// `and` and inside comparisons, but *not* under `or`/`not`, where the guard
/// could be weakened away. A false negative only over-rejects (the caller must
/// make the update relative or version-guarded); it never lets a blind set
/// through.
fn has_guard_predicate(pred: &Expr, def: &TableDef) -> bool {
    match pred {
        Expr::And(items) => items.iter().any(|p| has_guard_predicate(p, def)),
        Expr::Cmp { lhs, rhs, .. } => guards_column(lhs, def) || guards_column(rhs, def),
        Expr::Between { expr, .. } => guards_column(expr, def),
        Expr::InList { expr, .. } => guards_column(expr, def),
        _ => false,
    }
}

/// Whether `expr` is a reference to a guarded or rowversion column of `def`.
fn guards_column(expr: &Expr, def: &TableDef) -> bool {
    let Expr::Column { table, column } = expr else {
        return false;
    };
    // Unqualified, or qualified by the table's own name — an update targets a
    // single table with no alias.
    if let Some(t) = table {
        if t != &def.name {
            return false;
        }
    }
    def.columns
        .iter()
        .any(|c| c.name == *column && (c.update == UpdatePolicy::Guarded || c.rowversion))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use proto::{ClauseSelect, JoinSpec, Stage, TableRef};

    /// An in-memory schema source for unit tests.
    struct MapSchema(HashMap<String, TableDef>);

    impl MapSchema {
        fn new(tables: Vec<TableDef>) -> Self {
            MapSchema(tables.into_iter().map(|t| (t.name.clone(), t)).collect())
        }
    }

    impl SchemaView for MapSchema {
        fn table(&self, table: &str) -> std::result::Result<Option<TableDef>, CatalogError> {
            Ok(self.0.get(table).cloned())
        }
    }

    fn accounts() -> TableDef {
        TableDef::new(
            "accounts",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("balance", TypeKind::I64)
                    .not_null()
                    .guarded(),
                ColumnDef::new("owner", TypeKind::Text),
                ColumnDef::new("version", TypeKind::I64).rowversion(),
            ],
            vec!["id"],
        )
    }

    fn users() -> TableDef {
        TableDef::new(
            "users",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("name", TypeKind::Text).not_null(),
                ColumnDef::new("meta", TypeKind::Json),
            ],
            vec!["id"],
        )
    }

    fn orders() -> TableDef {
        TableDef::new(
            "orders",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("user_id", TypeKind::I64),
                ColumnDef::new("amount", TypeKind::I64).not_null(),
            ],
            vec!["id"],
        )
    }

    fn schema() -> MapSchema {
        MapSchema::new(vec![accounts(), users(), orders()])
    }

    fn col(name: &str) -> Expr {
        Expr::Column {
            table: None,
            column: name.to_string(),
        }
    }

    fn qcol(t: &str, name: &str) -> Expr {
        Expr::Column {
            table: Some(t.to_string()),
            column: name.to_string(),
        }
    }

    fn lit(n: i64) -> Expr {
        Expr::Literal(Value::I64(n))
    }

    fn cmp(op: CmpOp, l: Expr, r: Expr) -> Expr {
        Expr::Cmp {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }

    fn scan(table: &str) -> Select {
        Select::Pipeline(vec![Stage::Scan(TableRef {
            table: table.to_string(),
            alias: None,
        })])
    }

    // --- name resolution & typing ---

    #[test]
    fn scan_output_schema() {
        let out = validate_select(&scan("users"), &schema()).unwrap();
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "meta"]);
        // PK is not-null; `name` is not-null; `meta` is nullable.
        assert!(!out[0].nullable);
        assert!(!out[1].nullable);
        assert!(out[2].nullable);
    }

    #[test]
    fn unknown_table_is_rejected() {
        let err = validate_select(&scan("ghosts"), &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::UnknownTable { .. }));
    }

    #[test]
    fn unknown_column_is_rejected() {
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "users".into(),
                alias: None,
            }),
            Stage::Match(cmp(CmpOp::Eq, col("nope"), lit(1))),
        ]);
        let err = validate_select(&sel, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::UnknownColumn { .. }));
    }

    #[test]
    fn ambiguous_column_across_join() {
        // Both `users` and `orders` have `id`; an unqualified `id` is ambiguous.
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "users".into(),
                alias: None,
            }),
            Stage::Join(JoinSpec {
                kind: JoinKind::Inner,
                table: TableRef {
                    table: "orders".into(),
                    alias: None,
                },
                on: Some(cmp(
                    CmpOp::Eq,
                    qcol("users", "id"),
                    qcol("orders", "user_id"),
                )),
            }),
            Stage::Match(cmp(CmpOp::Eq, col("id"), lit(1))),
        ]);
        let err = validate_select(&sel, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::AmbiguousColumn { .. }));
    }

    #[test]
    fn non_boolean_where_is_rejected() {
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "users".into(),
                alias: None,
            }),
            Stage::Match(col("name")),
        ]);
        let err = validate_select(&sel, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::NonBooleanPredicate { .. }));
    }

    #[test]
    fn incompatible_comparison_is_rejected() {
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "users".into(),
                alias: None,
            }),
            Stage::Match(cmp(CmpOp::Eq, col("name"), lit(1))),
        ]);
        let err = validate_select(&sel, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::IncompatibleComparison { .. }));
    }

    #[test]
    fn numeric_comparison_across_i64_f64_ok() {
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "orders".into(),
                alias: None,
            }),
            Stage::Match(cmp(
                CmpOp::Gt,
                col("amount"),
                Expr::Literal(Value::F64(1.5)),
            )),
        ]);
        validate_select(&sel, &schema()).unwrap();
    }

    #[test]
    fn json_vs_json_comparison_rejected() {
        let two_json = TableDef::new(
            "docs",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("a", TypeKind::Json),
                ColumnDef::new("b", TypeKind::Json),
            ],
            vec!["id"],
        );
        let schema = MapSchema::new(vec![two_json]);
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "docs".into(),
                alias: None,
            }),
            Stage::Match(cmp(CmpOp::Eq, col("a"), col("b"))),
        ]);
        let err = validate_select(&sel, &schema).unwrap_err();
        assert!(matches!(err, ValidateError::JsonNotOrderable { .. }));
    }

    #[test]
    fn left_join_makes_right_nullable() {
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "users".into(),
                alias: Some("u".into()),
            }),
            Stage::Join(JoinSpec {
                kind: JoinKind::Left,
                table: TableRef {
                    table: "orders".into(),
                    alias: Some("o".into()),
                },
                on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
            }),
            Stage::Project(vec![
                Projection::Expr(qcol("u", "name")),
                Projection::Expr(qcol("o", "amount")),
            ]),
        ]);
        let out = validate_select(&sel, &schema()).unwrap();
        assert_eq!(out[0].name, "name");
        assert!(!out[0].nullable); // users.name is not-null
        assert_eq!(out[1].name, "amount");
        assert!(out[1].nullable); // orders.amount not-null, but LEFT-nullable
    }

    #[test]
    fn aggregate_output_and_passthrough() {
        // group by user_id, sum(amount) as spent — both `spent` and base
        // columns are visible afterward (D24).
        let sel = Select::Clause(Box::new(ClauseSelect {
            from: Some(TableRef {
                table: "orders".into(),
                alias: None,
            }),
            group_by: vec![col("user_id")],
            select: Some(vec![
                Projection::Expr(col("user_id")),
                Projection::Aliased {
                    name: "spent".into(),
                    expr: Expr::Agg {
                        func: AggFunc::Sum,
                        arg: Box::new(col("amount")),
                    },
                },
            ]),
            ..ClauseSelect::default()
        }));
        let out = validate_select(&sel, &schema()).unwrap();
        assert_eq!(out[0].name, "user_id");
        assert_eq!(out[1].name, "spent");
        assert_eq!(out[1].kind, Some(TypeKind::I64));
        assert!(out[1].nullable); // sum of an empty group is null
    }

    #[test]
    fn aggregate_in_filter_is_rejected() {
        // A raw aggregate in a WHERE has no group to attach to.
        let sel = Select::Pipeline(vec![
            Stage::Scan(TableRef {
                table: "orders".into(),
                alias: None,
            }),
            Stage::Match(cmp(
                CmpOp::Gt,
                Expr::Agg {
                    func: AggFunc::Sum,
                    arg: Box::new(col("amount")),
                },
                lit(1),
            )),
        ]);
        let err = validate_select(&sel, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::AggregateNotAllowed));
    }

    // --- §6 safety rules: writes ---

    fn update(selector: Option<Selector>, set: Vec<(&str, Expr)>, unconditional: bool) -> Update {
        Update {
            table: "accounts".into(),
            selector,
            set: set.into_iter().map(|(n, e)| (n.to_string(), e)).collect(),
            unconditional,
        }
    }

    #[test]
    fn update_without_selector_is_rejected() {
        let u = update(
            None,
            vec![("owner", Expr::Literal(Value::Text("x".into())))],
            false,
        );
        let err = validate_update(&u, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::MissingSelector));
    }

    #[test]
    fn delete_without_selector_is_rejected() {
        let d = Delete {
            table: "accounts".into(),
            selector: None,
        };
        let err = validate_delete(&d, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::MissingSelector));
    }

    #[test]
    fn guarded_blind_absolute_set_is_rejected() {
        // balance is guarded; a bare PK lookup carries no guard/version.
        let u = update(
            Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
            vec![("balance", lit(30))],
            false,
        );
        let err = validate_update(&u, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::GuardedBlindSet { .. }));
    }

    #[test]
    fn guarded_relative_update_is_allowed() {
        // balance = balance - 50 WHERE balance >= 50 (the bank scenario).
        let u = update(
            Some(Selector::Where(cmp(CmpOp::Gte, col("balance"), lit(50)))),
            vec![(
                "balance",
                Expr::Arith {
                    op: ArithOp::Sub,
                    lhs: Box::new(col("balance")),
                    rhs: Box::new(lit(50)),
                },
            )],
            false,
        );
        validate_update(&u, &schema()).unwrap();
    }

    #[test]
    fn guarded_version_guarded_absolute_set_is_allowed() {
        // balance = 30 WHERE id = 1 AND version = 5 (optimistic path).
        let u = update(
            Some(Selector::Where(Expr::And(vec![
                cmp(CmpOp::Eq, col("id"), lit(1)),
                cmp(CmpOp::Eq, col("version"), lit(5)),
            ]))),
            vec![("balance", lit(30))],
            false,
        );
        validate_update(&u, &schema()).unwrap();
    }

    #[test]
    fn unconditional_never_rescues_a_guarded_column() {
        let u = update(
            Some(Selector::Where(cmp(CmpOp::Gte, col("balance"), lit(50)))),
            vec![("balance", lit(30))],
            true,
        );
        let err = validate_update(&u, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::GuardedBlindSet { .. }));
    }

    #[test]
    fn free_column_absolute_set_is_allowed() {
        let u = update(
            Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
            vec![("owner", Expr::Literal(Value::Text("ada".into())))],
            true,
        );
        validate_update(&u, &schema()).unwrap();
    }

    #[test]
    fn updating_pk_is_rejected() {
        let u = update(
            Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
            vec![("id", lit(2))],
            false,
        );
        let err = validate_update(&u, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::PkImmutable { .. }));
    }

    #[test]
    fn writing_engine_managed_column_is_rejected() {
        let u = update(
            Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
            vec![("version", lit(9))],
            false,
        );
        let err = validate_update(&u, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::EngineManagedWrite { .. }));
    }

    #[test]
    fn assign_type_mismatch_is_rejected() {
        let u = update(
            Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
            vec![("owner", lit(7))], // owner is text, 7 is i64
            true,
        );
        let err = validate_update(&u, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::AssignTypeMismatch { .. }));
    }

    #[test]
    fn insert_unknown_column_is_rejected() {
        let ins = Insert {
            table: "users".into(),
            rows: vec![vec![("nope".into(), Value::I64(1))]],
        };
        let err = validate_insert(&ins, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::UnknownColumn { .. }));
    }

    #[test]
    fn insert_engine_managed_column_is_rejected() {
        let ins = Insert {
            table: "accounts".into(),
            rows: vec![vec![("version".into(), Value::I64(1))]],
        };
        let err = validate_insert(&ins, &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::EngineManagedWrite { .. }));
    }

    #[test]
    fn transaction_validates_every_op() {
        let good = Request::Update(update(
            Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
            vec![("owner", Expr::Literal(Value::Text("x".into())))],
            true,
        ));
        let bad = Request::Delete(Delete {
            table: "accounts".into(),
            selector: None,
        });
        let err = validate(&Request::Transaction(vec![good, bad]), &schema()).unwrap_err();
        assert!(matches!(err, ValidateError::MissingSelector));
    }
}
