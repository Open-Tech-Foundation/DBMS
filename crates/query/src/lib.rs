//! `query` — surface lowering, validator, planner, executor, write path.
//!
//! Phase 8 delivered the surface → IR [`lower`]ing: both surface forms fold
//! into one logical-plan tree (the clause form desugars into its fixed-order
//! pipeline first, so equivalence is by construction).
//!
//! Phase 9 adds the [`validate`]or: name resolution, expression type-checking,
//! and `SPEC.md` §6 safety-rule enforcement over the IR (reads) and DML AST
//! (writes). The planner (index/join/stage choices), executor (pull-based
//! operators), write path (guarded read-check-write, optimistic version), and
//! EXPLAIN follow.

mod eval;
mod exec;
mod lower;
mod validate;

use common::{CategorizedError, ErrorCategory};

pub use eval::{eval, eval_predicate, BoundColumn, EvalError, Shape};
pub use exec::{execute as execute_reference, ExecError, Relation};
pub use lower::{lower, LowerError};
pub use validate::{
    validate, validate_select, OutputColumn, OutputSchema, SchemaView, ValidateError, Validated,
};

/// Errors raised by the query layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueryError {
    /// An error from the protocol layer.
    #[error(transparent)]
    Proto(#[from] proto::ProtoError),
    /// A select that does not lower into the IR.
    #[error(transparent)]
    Lower(#[from] LowerError),
    /// A request that fails validation (names, types, `SPEC.md` §6 rules).
    #[error(transparent)]
    Validate(#[from] ValidateError),
    /// A runtime expression-evaluation failure (overflow, div-by-zero, cast).
    #[error(transparent)]
    Eval(#[from] EvalError),
    /// An error executing a plan.
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// An error from the catalog layer.
    #[error(transparent)]
    Catalog(#[from] catalog::CatalogError),
    /// An error from the index layer.
    #[error(transparent)]
    Index(#[from] index::IndexError),
    /// An error from the transaction layer.
    #[error(transparent)]
    Txn(#[from] txn::TxnError),
}

impl CategorizedError for QueryError {
    fn category(&self) -> ErrorCategory {
        match self {
            QueryError::Proto(e) => e.category(),
            QueryError::Lower(e) => e.category(),
            QueryError::Validate(e) => e.category(),
            QueryError::Eval(e) => e.category(),
            QueryError::Exec(e) => e.category(),
            QueryError::Catalog(e) => e.category(),
            QueryError::Index(e) => e.category(),
            QueryError::Txn(e) => e.category(),
        }
    }
}

/// Result alias for query operations.
pub type Result<T> = std::result::Result<T, QueryError>;
