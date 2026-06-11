//! `proto` — query protocol, AST, and IR.
//!
//! The wire boundary of the engine (`SPEC.md` §5, `ARCHITECTURE.md` §3.7):
//!
//! - Wire-level decoding — the hardened MessagePack document layer
//!   ([`Doc`], [`DecodeLimits`]): size/depth/node limits enforced before
//!   allocation, reserved/ext bytes, non-string and duplicate map keys, and
//!   trailing bytes all rejected.
//! - The typed AST for both surface forms (pipeline + clause), expressions,
//!   and DML; strict `Doc → Ast` mapping that rejects unknown nodes and
//!   fields, plus the canonical `Ast → Doc` encoding.
//! - The logical-plan IR ([`Plan`]) both surfaces lower into (lowering
//!   itself lives in the `query` crate).
//! - Result encoding (`SPEC.md` §5.6), the error-result shape, and the
//!   opaque keyset cursor-token envelope.
//!
//! Queries are **data, never code**: any node type outside the grammar is a
//! typed `Validation` error.

mod ast;
mod ir;
mod result;
mod wire;

use common::{CategorizedError, ErrorCategory};

pub use ast::{
    decode_request, encode_request, request_from_doc, AggFunc, ArithOp, ClauseSelect, CmpOp,
    Delete, Dir, Expr, Insert, JoinKind, JoinSpec, Projection, Request, Select, Selector, SortKey,
    Stage, TableRef, Update,
};
pub use ir::Plan;
pub use result::{
    decode_cursor_token, encode_cursor_token, encode_error_result, encode_result, QueryResult,
};
pub use wire::{decode_doc, encode_doc, DecodeLimits, Doc};

/// The protocol version this build speaks. Requests may carry a `v` field
/// (missing means version 1); any other version is rejected. Results always
/// carry `v`.
pub const PROTOCOL_VERSION: i64 = 1;

/// Errors raised by the protocol layer. All are `Validation`-category: the
/// input was malformed, oversized, or outside the grammar (`SPEC.md` §9).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProtoError {
    /// An error from the type/encoding layer.
    #[error(transparent)]
    Type(#[from] types::TypeError),
    /// The message exceeds the configured size cap.
    #[error("message of {len} bytes exceeds the limit of {max}")]
    MessageTooLarge {
        /// The message length.
        len: usize,
        /// The enforced limit.
        max: usize,
    },
    /// The bytes end mid-value.
    #[error("message is truncated")]
    Truncated,
    /// Bytes left over after the message.
    #[error("{count} trailing byte(s) after the message")]
    TrailingBytes {
        /// How many bytes were left.
        count: usize,
    },
    /// Nesting beyond the configured depth cap.
    #[error("nesting exceeds the depth limit of {max}")]
    TooDeep {
        /// The enforced limit.
        max: usize,
    },
    /// More nodes than the configured node cap.
    #[error("message exceeds the node-count limit")]
    TooManyNodes,
    /// An unsigned integer too large for `i64`.
    #[error("integer {found} does not fit in i64")]
    IntOutOfRange {
        /// The value found on the wire.
        found: u64,
    },
    /// A str payload that is not valid UTF-8.
    #[error("str payload is not valid UTF-8")]
    InvalidUtf8,
    /// An ext-family value (never part of the protocol).
    #[error("ext types are not allowed")]
    ExtNotAllowed,
    /// The reserved head byte `0xC1`.
    #[error("reserved head byte 0xc1")]
    ReservedByte,
    /// A map key that is not a string.
    #[error("map keys must be strings")]
    NonStringKey,
    /// The same key twice in one map.
    #[error("duplicate map key {key:?}")]
    DuplicateKey {
        /// The repeated key.
        key: String,
    },
    /// A node name outside the grammar (unknown op, stage, or expression).
    #[error("unknown {context} node {name:?}")]
    UnknownNode {
        /// Where the node appeared (e.g. "request", "stage", "expression").
        context: &'static str,
        /// The unrecognized name.
        name: String,
    },
    /// A map field not defined for the node.
    #[error("unknown field {name:?} on {node}")]
    UnknownField {
        /// The node carrying the field.
        node: &'static str,
        /// The unrecognized field name.
        name: String,
    },
    /// A required field is absent.
    #[error("missing field {field:?} on {node}")]
    MissingField {
        /// The node missing the field.
        node: &'static str,
        /// The required field name.
        field: &'static str,
    },
    /// A field whose value has the wrong shape.
    #[error("field {field:?} on {node} must be {expected}, found {found}")]
    WrongShape {
        /// The node carrying the field.
        node: &'static str,
        /// The field name.
        field: &'static str,
        /// The shape the grammar requires.
        expected: &'static str,
        /// The shape found on the wire.
        found: &'static str,
    },
    /// A field whose value is the right shape but outside the allowed set.
    #[error("field {field:?} on {node} has invalid value {found:?}")]
    InvalidValue {
        /// The node carrying the field.
        node: &'static str,
        /// The field name.
        field: &'static str,
        /// The offending value, rendered.
        found: String,
    },
    /// A protocol version this build does not speak.
    #[error("unsupported protocol version {found}")]
    UnsupportedVersion {
        /// The version found on the wire.
        found: i64,
    },
    /// A cursor token that fails its integrity check.
    #[error("malformed cursor token")]
    BadCursorToken,
}

impl CategorizedError for ProtoError {
    fn category(&self) -> ErrorCategory {
        match self {
            ProtoError::Type(e) => e.category(),
            // Everything else is malformed/oversized input (SPEC §9).
            _ => ErrorCategory::Validation,
        }
    }
}

/// Result alias for protocol operations.
pub type Result<T> = std::result::Result<T, ProtoError>;
