//! Caller-supplied policies for conditional multi-row writes.
//!
//! The query layer builds these from a validated `update`/`delete` and hands
//! them to [`Catalog::update_where`](crate::Catalog::update_where) /
//! [`delete_where`](crate::Catalog::delete_where). The catalog runs them
//! against **live committed rows inside the writer** (`SPEC.md` §6 rule 3), so
//! a guarded read-check-write is one atomic step the client cannot split —
//! this is what serializes the headline bank scenario and gives
//! first-committer-wins on the optimistic path.
//!
//! Rows are presented in schema column order; a policy resolves column
//! references against the [`TableDef`]. Errors are boxed as
//! [`CategorizedError`] so their `SPEC.md` §9 category survives the trip across
//! the writer thread and back.

use common::CategorizedError;
use types::Value;

use crate::TableDef;

/// A policy error, carrying its taxonomy category across the writer thread.
pub type PolicyError = Box<dyn CategorizedError + Send + Sync>;

/// Selects rows for a conditional write. `Send + 'static` so the policy can
/// travel to the writer thread inside the job.
pub trait RowFilter: Send + 'static {
    /// Whether `row` (schema column order) is targeted by the write.
    fn matches(&self, def: &TableDef, row: &[Value]) -> Result<bool, PolicyError>;
}

/// Computes the new absolute column values for a matched row — the evaluated
/// `set` list of an update.
pub trait RowUpdater: RowFilter {
    /// The `(column, new value)` pairs for a matched row.
    fn new_values(
        &self,
        def: &TableDef,
        row: &[Value],
    ) -> Result<Vec<(String, Value)>, PolicyError>;
}
