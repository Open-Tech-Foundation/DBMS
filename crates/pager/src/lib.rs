//! `pager` — fixed-size paged file I/O.
//!
//! Owns the on-disk page format and checksums, an LRU page cache, the
//! double-buffered meta pages, and the free-page allocator. Implemented in
//! Phase 2; this is the Phase 1 scaffold (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

/// Errors raised by the pager.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PagerError {
    /// An underlying I/O failure from the backend.
    #[error(transparent)]
    Io(#[from] common::IoError),
}

impl CategorizedError for PagerError {
    fn category(&self) -> ErrorCategory {
        match self {
            PagerError::Io(e) => e.category(),
        }
    }
}

/// Result alias for pager operations.
pub type Result<T> = std::result::Result<T, PagerError>;
