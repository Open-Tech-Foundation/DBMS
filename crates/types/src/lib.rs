//! `types` — value model and encodings.
//!
//! The `Value` model for all v1 types, the order-preserving key encoding, row
//! encoding, MessagePack value mapping, the UUIDv7 generator, and json
//! well-formedness. Implemented in Phase 5; this is the Phase 1 scaffold
//! (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

/// Errors raised by the type/encoding layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TypeError {
    /// A value did not satisfy its declared type or encoding contract.
    #[error("value does not satisfy the type contract")]
    Invalid,
}

impl CategorizedError for TypeError {
    fn category(&self) -> ErrorCategory {
        // Type/encoding failures are surfaced as validation errors (SPEC §9).
        ErrorCategory::Validation
    }
}

/// Result alias for type operations.
pub type Result<T> = std::result::Result<T, TypeError>;
