//! The shared error taxonomy (`SPEC.md` §9).

/// The seven actionable error categories the engine surfaces.
///
/// Every crate keeps its own `thiserror` enum; each maps onto exactly one of
/// these via [`CategorizedError`]. The public `core` crate aggregates the
/// crate-local errors and exposes this taxonomy to integrators.
///
/// # Examples
///
/// ```
/// use common::ErrorCategory;
///
/// assert_eq!(ErrorCategory::Conflict.as_str(), "conflict");
/// assert_ne!(ErrorCategory::Io, ErrorCategory::Corruption);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCategory {
    /// Malformed/oversized query, type error, or safety-rule violation.
    Validation,
    /// PK / UNIQUE / CHECK / NOT NULL violation.
    Constraint,
    /// Optimistic version mismatch (first-committer-wins).
    Conflict,
    /// A required row, table, or index was not found.
    NotFound,
    /// A checksum failed; corrupt data is surfaced, never served.
    Corruption,
    /// A per-query resource cap was exceeded.
    ResourceLimit,
    /// An underlying I/O failure.
    Io,
}

impl ErrorCategory {
    /// A stable, lowercase identifier for the category.
    ///
    /// # Examples
    ///
    /// ```
    /// use common::ErrorCategory;
    ///
    /// assert_eq!(ErrorCategory::Validation.as_str(), "validation");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCategory::Validation => "validation",
            ErrorCategory::Constraint => "constraint",
            ErrorCategory::Conflict => "conflict",
            ErrorCategory::NotFound => "not_found",
            ErrorCategory::Corruption => "corruption",
            ErrorCategory::ResourceLimit => "resource_limit",
            ErrorCategory::Io => "io",
        }
    }
}

impl std::fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Implemented by every crate-local error so it can be mapped to the public
/// [`ErrorCategory`] taxonomy without losing its typed identity.
///
/// # Examples
///
/// ```
/// use common::{CategorizedError, ErrorCategory, IoError};
///
/// let err = IoError::OutOfBounds { offset: 8, requested: 4, size: 8 };
/// assert_eq!(err.category(), ErrorCategory::Io);
/// ```
pub trait CategorizedError: std::error::Error {
    /// The taxonomy category this error belongs to.
    fn category(&self) -> ErrorCategory;
}
