//! `proto` — query protocol, AST, and IR.
//!
//! AST types for both surface forms (pipeline + clause), the logical-plan IR,
//! the hardened MessagePack decoder (depth/node/size limits, unknown-node
//! rejection), and result encoding. Implemented in Phase 8; this is the
//! Phase 1 scaffold (error taxonomy wiring only).

use common::{CategorizedError, ErrorCategory};

/// Errors raised by the protocol layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtoError {
    /// An error from the type/encoding layer.
    #[error(transparent)]
    Type(#[from] types::TypeError),
    /// A malformed, oversized, or over-nested protocol message.
    #[error("malformed protocol message")]
    Malformed,
}

impl CategorizedError for ProtoError {
    fn category(&self) -> ErrorCategory {
        match self {
            ProtoError::Type(e) => e.category(),
            // Malformed/oversized input is a validation failure (SPEC §9).
            ProtoError::Malformed => ErrorCategory::Validation,
        }
    }
}

/// Result alias for protocol operations.
pub type Result<T> = std::result::Result<T, ProtoError>;
