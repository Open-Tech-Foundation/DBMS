//! The logical-plan IR (`ARCHITECTURE.md` §3.7) — the single normalized
//! operator tree both surface forms lower into. The validator, planner, and
//! executor only ever touch this tree; there is no second engine for the
//! second surface.
//!
//! The IR is internal (never wire-encoded), so its `PartialEq` is exact
//! structural equality — the property the clause↔pipeline equivalence tests
//! assert.

use types::Value;

use crate::ast::{Expr, JoinKind, Projection, SortKey};

/// One logical-plan operator. Each node owns its input(s); a query is a
/// single root `Plan`.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// A full scan of a base table. The only access path lowering emits.
    Scan {
        /// The table name.
        table: String,
        /// The alias rows are referenced by, if any.
        alias: Option<String>,
    },
    /// An index access path. Emitted only by the planner (Phase 9) when it
    /// proves an index applies; lowering never produces this node, and its
    /// fields are finalized with the planner.
    IndexScan {
        /// The base table.
        table: String,
        /// The alias rows are referenced by, if any.
        alias: Option<String>,
        /// The index used.
        index: String,
        /// Equality values for a leading prefix of the indexed columns
        /// (empty = scan the whole index in key order).
        prefix: Vec<Value>,
    },
    /// A primary-key point lookup: a direct seek to the single row whose full
    /// primary key equals `key`. Emitted only by the planner, when a filter pins
    /// every primary-key column to an equality value. The fastest access path —
    /// one base-tree `get` instead of a full scan and filter.
    PkLookup {
        /// The base table.
        table: String,
        /// The alias rows are referenced by, if any.
        alias: Option<String>,
        /// The full primary key, in key-column order.
        key: Vec<Value>,
    },
    /// Keep rows where the predicate is true (`match` / WHERE / HAVING).
    Filter {
        /// The input plan.
        input: Box<Plan>,
        /// The predicate (SQL three-valued logic: only `true` keeps a row).
        pred: Expr,
    },
    /// A join of two inputs.
    Join {
        /// INNER, LEFT, or CROSS.
        kind: JoinKind,
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
        /// The join predicate (`None` for cross joins).
        on: Option<Expr>,
    },
    /// Group rows and compute aggregates.
    Aggregate {
        /// The input plan.
        input: Box<Plan>,
        /// Grouping keys (empty = one global group).
        by: Vec<Expr>,
        /// Named aggregate outputs (each expression is an `Expr::Agg`).
        aggs: Vec<(String, Expr)>,
    },
    /// The output list (the SELECT list).
    Project {
        /// The input plan.
        input: Box<Plan>,
        /// The projected items.
        items: Vec<Projection>,
    },
    /// Drop duplicate output rows.
    Distinct {
        /// The input plan.
        input: Box<Plan>,
    },
    /// Order rows (ORDER BY).
    Sort {
        /// The input plan.
        input: Box<Plan>,
        /// The sort keys, major first.
        keys: Vec<SortKey>,
    },
    /// Cap and/or skip rows.
    Limit {
        /// The input plan.
        input: Box<Plan>,
        /// Maximum rows, if capped.
        limit: Option<u64>,
        /// Rows skipped first.
        offset: u64,
    },
    /// Resume keyset pagination from an opaque continuation token.
    Cursor {
        /// The input plan.
        input: Box<Plan>,
        /// The token (validated by [`crate::decode_cursor_token`]).
        token: Vec<u8>,
    },
}
