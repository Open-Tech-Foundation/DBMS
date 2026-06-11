//! The schema model (`SPEC.md` §4): tables, columns, constraints, defaults,
//! generators, update policies — and the definition-time validation rules.

use types::{TypeKind, Value};

use crate::{CatalogError, Result};

/// Per-column update policy (`SPEC.md` §4.2). `Guarded` is persisted and
/// readable from Phase 6; the Phase 9 validator enforces it (a blind absolute
/// set on a guarded column is rejected there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePolicy {
    /// A plain absolute set is allowed (last-write-wins).
    Free,
    /// Only relative or guard-carrying updates are allowed.
    Guarded,
}

/// A column default: a constant value or a generator (`SPEC.md` §4.1).
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultSpec {
    /// A constant value (type-checked against the column at DDL time).
    Value(Value),
    /// The current timestamp (`now`).
    Now,
    /// A fresh time-ordered UUID (`uuid_v7`).
    UuidV7,
}

/// One column definition.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// The column name (unique within the table).
    pub name: String,
    /// The declared type.
    pub kind: TypeKind,
    /// `false` = NOT NULL. Primary-key columns are not-null regardless.
    pub nullable: bool,
    /// UNIQUE constraint.
    pub unique: bool,
    /// Applied when the column is omitted on insert.
    pub default: Option<DefaultSpec>,
    /// Engine-assigned ascending integer key (requires `i64`).
    pub auto_increment: bool,
    /// Engine-incremented version counter (requires `i64`; engine-managed).
    pub rowversion: bool,
    /// Refreshed to the current timestamp on every update (requires
    /// `timestamp`; engine-managed; also stamped on insert).
    pub on_update_now: bool,
    /// The update policy (`SPEC.md` §4.2).
    pub update: UpdatePolicy,
}

impl ColumnDef {
    /// A plain nullable column of `kind` with policy `free` and no constraints.
    pub fn new(name: impl Into<String>, kind: TypeKind) -> Self {
        ColumnDef {
            name: name.into(),
            kind,
            nullable: true,
            unique: false,
            default: None,
            auto_increment: false,
            rowversion: false,
            on_update_now: false,
            update: UpdatePolicy::Free,
        }
    }

    /// Mark NOT NULL.
    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    /// Mark UNIQUE.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Set a constant default.
    pub fn default_value(mut self, value: Value) -> Self {
        self.default = Some(DefaultSpec::Value(value));
        self
    }

    /// Default to the current timestamp.
    pub fn default_now(mut self) -> Self {
        self.default = Some(DefaultSpec::Now);
        self
    }

    /// Default to a fresh UUIDv7.
    pub fn default_uuid_v7(mut self) -> Self {
        self.default = Some(DefaultSpec::UuidV7);
        self
    }

    /// Mark auto-increment.
    pub fn auto_increment(mut self) -> Self {
        self.auto_increment = true;
        self
    }

    /// Mark as the table's rowversion column.
    pub fn rowversion(mut self) -> Self {
        self.rowversion = true;
        self
    }

    /// Refresh to `now` on every update.
    pub fn on_update_now(mut self) -> Self {
        self.on_update_now = true;
        self
    }

    /// Set the `guarded` update policy.
    pub fn guarded(mut self) -> Self {
        self.update = UpdatePolicy::Guarded;
        self
    }
}

/// A comparison operator in a [`CheckExpr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `≠`
    Ne,
    /// `<`
    Lt,
    /// `≤`
    Lte,
    /// `>`
    Gt,
    /// `≥`
    Gte,
}

/// A CHECK expression over the row.
///
/// **Provisional** (Phase 6): a column-vs-literal comparison subset with
/// boolean combinators — enough to express constraints like
/// `balance >= 0`. The Phase 8/9 expression engine supersedes it.
///
/// Evaluation follows SQL three-valued logic: a NULL operand makes a
/// comparison *unknown*, and a CHECK is violated only when it evaluates to
/// **false** — unknown passes.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckExpr {
    /// `column <op> literal`.
    Cmp {
        /// The column name.
        col: String,
        /// The comparison operator.
        op: CmpOp,
        /// The literal to compare against (same kind as the column).
        value: Value,
    },
    /// `column IS NULL`.
    IsNull {
        /// The column name.
        col: String,
    },
    /// `column IS NOT NULL`.
    IsNotNull {
        /// The column name.
        col: String,
    },
    /// Every operand true (3VL: false dominates, then unknown).
    And(Vec<CheckExpr>),
    /// Any operand true (3VL: true dominates, then unknown).
    Or(Vec<CheckExpr>),
    /// Logical negation (unknown stays unknown).
    Not(Box<CheckExpr>),
}

/// The deepest CHECK nesting accepted (validation and decode).
pub(crate) const MAX_CHECK_DEPTH: usize = 16;

impl CheckExpr {
    /// Evaluate against a full row (`row[i]` is column `i`); `None` = unknown.
    pub fn eval(&self, table: &TableDef, row: &[Value]) -> Option<bool> {
        match self {
            CheckExpr::Cmp { col, op, value } => {
                let cell = table.col_index(col).and_then(|i| row.get(i))?;
                if matches!(cell, Value::Null) {
                    return None;
                }
                let ord = cell.logical_cmp(value);
                Some(match op {
                    CmpOp::Eq => ord.is_eq(),
                    CmpOp::Ne => ord.is_ne(),
                    CmpOp::Lt => ord.is_lt(),
                    CmpOp::Lte => ord.is_le(),
                    CmpOp::Gt => ord.is_gt(),
                    CmpOp::Gte => ord.is_ge(),
                })
            }
            CheckExpr::IsNull { col } => {
                let cell = table.col_index(col).and_then(|i| row.get(i))?;
                Some(matches!(cell, Value::Null))
            }
            CheckExpr::IsNotNull { col } => {
                let cell = table.col_index(col).and_then(|i| row.get(i))?;
                Some(!matches!(cell, Value::Null))
            }
            CheckExpr::And(items) => {
                let mut unknown = false;
                for item in items {
                    match item.eval(table, row) {
                        Some(false) => return Some(false),
                        None => unknown = true,
                        Some(true) => {}
                    }
                }
                if unknown {
                    None
                } else {
                    Some(true)
                }
            }
            CheckExpr::Or(items) => {
                let mut unknown = false;
                for item in items {
                    match item.eval(table, row) {
                        Some(true) => return Some(true),
                        None => unknown = true,
                        Some(false) => {}
                    }
                }
                if unknown {
                    None
                } else {
                    Some(false)
                }
            }
            CheckExpr::Not(inner) => inner.eval(table, row).map(|b| !b),
        }
    }

    fn validate(&self, table: &TableDef, depth: usize) -> Result<()> {
        if depth > MAX_CHECK_DEPTH {
            return Err(invalid("check expression nests too deeply"));
        }
        match self {
            CheckExpr::Cmp { col, value, .. } => {
                let Some(column) = table.columns.iter().find(|c| c.name == *col) else {
                    return Err(CatalogError::UnknownColumn {
                        table: table.name.clone(),
                        column: col.clone(),
                    });
                };
                match value.kind() {
                    None => Err(invalid(
                        "check comparisons against null never hold; use is_null",
                    )),
                    Some(kind) if kind != column.kind => Err(invalid(
                        "check literal kind does not match the column's type",
                    )),
                    Some(_) => Ok(()),
                }
            }
            CheckExpr::IsNull { col } | CheckExpr::IsNotNull { col } => {
                if table.col_index(col).is_none() {
                    return Err(CatalogError::UnknownColumn {
                        table: table.name.clone(),
                        column: col.clone(),
                    });
                }
                Ok(())
            }
            CheckExpr::And(items) | CheckExpr::Or(items) => {
                if items.is_empty() {
                    return Err(invalid("empty boolean combinator in check"));
                }
                for item in items {
                    item.validate(table, depth + 1)?;
                }
                Ok(())
            }
            CheckExpr::Not(inner) => inner.validate(table, depth + 1),
        }
    }
}

/// One secondary-index definition.
///
/// A `unique` column on a table implicitly creates a single-column unique
/// index named [`implicit_index_name`]`(table, column)` — that index *is* the
/// constraint's enforcement (`SPEC.md` §4.1) and cannot be dropped directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    /// The index name (unique within its table).
    pub name: String,
    /// The indexed column names, in key order.
    pub columns: Vec<String>,
    /// Whether the indexed columns must be unique across rows.
    pub unique: bool,
}

impl IndexDef {
    /// A non-unique index over `columns`.
    pub fn new(name: impl Into<String>, columns: Vec<impl Into<String>>) -> Self {
        IndexDef {
            name: name.into(),
            columns: columns.into_iter().map(Into::into).collect(),
            unique: false,
        }
    }

    /// Mark the index unique.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }
}

/// The auto-generated name of the index backing a `unique` column.
pub fn implicit_index_name(table: &str, column: &str) -> String {
    format!("uniq_{table}_{column}")
}

/// One table definition.
#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    /// The table name.
    pub name: String,
    /// The columns, in declaration (and row-encoding) order.
    pub columns: Vec<ColumnDef>,
    /// Primary-key column names, in key order. Required, non-empty.
    pub pk: Vec<String>,
    /// CHECK constraints over the row.
    pub checks: Vec<CheckExpr>,
    /// Secondary indexes (user-defined and implicit `unique` backings).
    pub indexes: Vec<IndexDef>,
}

impl TableDef {
    /// A table over `columns` keyed by `pk`.
    pub fn new(
        name: impl Into<String>,
        columns: Vec<ColumnDef>,
        pk: Vec<impl Into<String>>,
    ) -> Self {
        TableDef {
            name: name.into(),
            columns,
            pk: pk.into_iter().map(Into::into).collect(),
            checks: Vec::new(),
            indexes: Vec::new(),
        }
    }

    /// Attach a CHECK constraint.
    pub fn check(mut self, expr: CheckExpr) -> Self {
        self.checks.push(expr);
        self
    }

    /// Attach a secondary index.
    pub fn index(mut self, index: IndexDef) -> Self {
        self.indexes.push(index);
        self
    }

    /// The position of `index` in [`indexes`](Self::indexes), if it exists.
    pub fn index_pos(&self, index: &str) -> Option<usize> {
        self.indexes.iter().position(|i| i.name == index)
    }

    /// Whether `index` is the implicit backing of a `unique` column (and so
    /// cannot be dropped while the column keeps the constraint).
    pub fn backs_unique_column(&self, index: &IndexDef) -> bool {
        index.unique
            && index.columns.len() == 1
            && index.columns.first().is_some_and(|col| {
                index.name == implicit_index_name(&self.name, col)
                    && self.columns.iter().any(|c| c.name == *col && c.unique)
            })
    }

    /// The index of `column` in [`columns`](Self::columns), if it exists.
    pub fn col_index(&self, column: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == column)
    }

    /// Whether `column` (by index) is part of the primary key.
    pub fn is_pk(&self, index: usize) -> bool {
        self.columns
            .get(index)
            .is_some_and(|c| self.pk.contains(&c.name))
    }

    /// Whether `column` (by index) may hold NULL: declared nullable and not
    /// part of the primary key (PK implies NOT NULL, `SPEC.md` §4.1).
    pub fn is_nullable(&self, index: usize) -> bool {
        self.columns
            .get(index)
            .is_some_and(|c| c.nullable && !self.is_pk(index))
    }

    /// Validate the definition (`PLAN.md` §Phase 6: PK required, no duplicate
    /// columns, and the per-feature typing rules).
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(invalid("table name must not be empty"));
        }
        if self.columns.is_empty() {
            return Err(invalid("a table needs at least one column"));
        }
        for (i, col) in self.columns.iter().enumerate() {
            if col.name.is_empty() {
                return Err(invalid("column names must not be empty"));
            }
            if self.columns[..i].iter().any(|c| c.name == col.name) {
                return Err(invalid("duplicate column name"));
            }
            if col.auto_increment && col.kind != TypeKind::I64 {
                return Err(invalid("auto_increment requires an i64 column"));
            }
            if col.rowversion && col.kind != TypeKind::I64 {
                return Err(invalid("rowversion requires an i64 column"));
            }
            if col.rowversion && (col.auto_increment || col.default.is_some() || col.on_update_now)
            {
                return Err(invalid("a rowversion column is engine-managed only"));
            }
            if col.on_update_now && col.kind != TypeKind::Timestamp {
                return Err(invalid("on_update: now requires a timestamp column"));
            }
            match &col.default {
                Some(DefaultSpec::Now) if col.kind != TypeKind::Timestamp => {
                    return Err(invalid("default now requires a timestamp column"));
                }
                Some(DefaultSpec::UuidV7) if col.kind != TypeKind::Uuid => {
                    return Err(invalid("default uuid_v7 requires a uuid column"));
                }
                Some(DefaultSpec::Value(v)) => match v.kind() {
                    None if !col.nullable => {
                        return Err(invalid("default null on a not-null column"));
                    }
                    Some(kind) if kind != col.kind => {
                        return Err(invalid("default value kind does not match the column"));
                    }
                    _ => {
                        if let Value::Json(doc) = v {
                            types::validate_json(doc)?;
                        }
                    }
                },
                _ => {}
            }
        }
        if self.columns.iter().filter(|c| c.auto_increment).count() > 1 {
            return Err(invalid("at most one auto_increment column per table"));
        }
        if self.columns.iter().filter(|c| c.rowversion).count() > 1 {
            return Err(invalid("at most one rowversion column per table"));
        }

        if self.pk.is_empty() {
            return Err(invalid("every table requires a primary key"));
        }
        for (i, name) in self.pk.iter().enumerate() {
            if self.pk[..i].contains(name) {
                return Err(invalid("duplicate primary-key column"));
            }
            let Some(col) = self.columns.iter().find(|c| c.name == *name) else {
                return Err(CatalogError::UnknownColumn {
                    table: self.name.clone(),
                    column: name.clone(),
                });
            };
            if col.kind == TypeKind::Json {
                return Err(invalid("json columns cannot be key components"));
            }
            if col.rowversion {
                return Err(invalid("a rowversion column cannot be a primary key"));
            }
        }

        for check in &self.checks {
            check.validate(self, 0)?;
        }

        for (i, index) in self.indexes.iter().enumerate() {
            if index.name.is_empty() {
                return Err(invalid("index names must not be empty"));
            }
            if self.indexes[..i].iter().any(|x| x.name == index.name) {
                return Err(invalid("duplicate index name"));
            }
            if index.columns.is_empty() {
                return Err(invalid("an index needs at least one column"));
            }
            for (j, name) in index.columns.iter().enumerate() {
                if index.columns[..j].contains(name) {
                    return Err(invalid("duplicate column within an index"));
                }
                let Some(col) = self.columns.iter().find(|c| c.name == *name) else {
                    return Err(CatalogError::UnknownColumn {
                        table: self.name.clone(),
                        column: name.clone(),
                    });
                };
                if col.kind == TypeKind::Json {
                    return Err(invalid("json columns cannot be indexed"));
                }
            }
        }
        Ok(())
    }

    /// Append the implicit unique index for every `unique` column that does
    /// not have its backing index yet. Run before persisting a definition so
    /// the constraint is always index-enforced (`SPEC.md` §4.1).
    pub(crate) fn materialize_implicit_indexes(&mut self) {
        let needed: Vec<IndexDef> = self
            .columns
            .iter()
            .filter(|c| c.unique)
            .map(|c| IndexDef {
                name: implicit_index_name(&self.name, &c.name),
                columns: vec![c.name.clone()],
                unique: true,
            })
            .filter(|idx| self.index_pos(&idx.name).is_none())
            .collect();
        self.indexes.extend(needed);
    }
}

fn invalid(reason: &str) -> CatalogError {
    CatalogError::InvalidSchema {
        reason: reason.to_string(),
    }
}
