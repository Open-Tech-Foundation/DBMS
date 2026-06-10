//! `types` — the value model and its encodings.
//!
//! - [`Value`] covers every v1 type (`SPEC.md` §3); [`Value::logical_cmp`] is
//!   the engine's total order.
//! - [`encode_key`] / [`decode_key`]: the **order-preserving key encoding** —
//!   bytewise comparison of encoded keys equals the logical comparison of the
//!   values, including composite keys, with nulls first.
//! - [`encode_row`] / [`decode_row`]: the (non-ordered, exactly round-tripping)
//!   row encoding.
//! - [`encode_value`] / [`decode_value`] / [`validate_json`]: the MessagePack
//!   wire mapping and the `json` well-formedness gate.
//! - [`UuidV7Gen`]: time-ordered UUIDs, strictly monotonic within a run.
//!
//! ```
//! use types::{encode_key, Value};
//!
//! let a = encode_key(&[Value::Text("alice".into()), Value::I64(-3)]).unwrap();
//! let b = encode_key(&[Value::Text("alice".into()), Value::I64(7)]).unwrap();
//! assert!(a < b); // bytewise order == logical order
//! ```

mod key;
mod msgpack;
mod row;
mod uuid;
mod value;

use common::{CategorizedError, ErrorCategory};

pub use key::{decode_key, encode_key};
pub use msgpack::{decode_value, encode_value, validate_json, MAX_JSON_DEPTH};
pub use row::{decode_row, encode_row, MAX_COLUMNS};
pub use uuid::{uuid_from_str, uuid_to_string, UuidV7Gen};
pub use value::{TypeKind, Value};

/// How stored key bytes can be malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyCorruption {
    /// The bytes end mid-value.
    Truncated,
    /// An unknown type tag.
    BadTag {
        /// The offending tag byte.
        tag: u8,
    },
    /// A bool payload other than 0 or 1.
    BadBool {
        /// The offending payload byte.
        byte: u8,
    },
    /// A `0x00` followed by neither the terminator nor the escape.
    BadEscape {
        /// The byte found after `0x00`.
        escape: u8,
    },
    /// A text payload that is not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for KeyCorruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyCorruption::Truncated => write!(f, "key bytes are truncated"),
            KeyCorruption::BadTag { tag } => write!(f, "unknown key type tag {tag:#04x}"),
            KeyCorruption::BadBool { byte } => write!(f, "invalid bool payload {byte:#04x}"),
            KeyCorruption::BadEscape { escape } => {
                write!(f, "invalid escape byte {escape:#04x} after 0x00")
            }
            KeyCorruption::InvalidUtf8 => write!(f, "text payload is not valid UTF-8"),
        }
    }
}

/// How stored row bytes can be malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RowCorruption {
    /// The bytes end mid-value.
    Truncated,
    /// An unknown value tag.
    BadTag {
        /// The offending tag byte.
        tag: u8,
    },
    /// A bool payload other than 0 or 1.
    BadBool {
        /// The offending payload byte.
        byte: u8,
    },
    /// Bytes left over after the declared column count.
    TrailingBytes {
        /// How many bytes were left.
        count: usize,
    },
    /// A text payload that is not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for RowCorruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RowCorruption::Truncated => write!(f, "row bytes are truncated"),
            RowCorruption::BadTag { tag } => write!(f, "unknown row value tag {tag:#04x}"),
            RowCorruption::BadBool { byte } => write!(f, "invalid bool payload {byte:#04x}"),
            RowCorruption::TrailingBytes { count } => {
                write!(f, "{count} trailing byte(s) after the last column")
            }
            RowCorruption::InvalidUtf8 => write!(f, "text payload is not valid UTF-8"),
        }
    }
}

/// How wire bytes can fail to be (well-formed) MessagePack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MsgpackError {
    /// The bytes end mid-value.
    Truncated,
    /// Bytes left over after the value.
    TrailingBytes {
        /// How many bytes were left.
        count: usize,
    },
    /// Nesting beyond [`MAX_JSON_DEPTH`].
    DepthExceeded {
        /// The enforced limit.
        max: usize,
    },
    /// The reserved head byte `0xC1`.
    Reserved,
    /// An ext-family value (not allowed in v1 documents).
    ExtNotAllowed,
    /// A str payload that is not valid UTF-8.
    InvalidUtf8,
    /// An unsigned integer too large for `i64`.
    IntOutOfRange {
        /// The value found on the wire.
        found: u64,
    },
}

impl std::fmt::Display for MsgpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MsgpackError::Truncated => write!(f, "messagepack bytes are truncated"),
            MsgpackError::TrailingBytes { count } => {
                write!(f, "{count} trailing byte(s) after the value")
            }
            MsgpackError::DepthExceeded { max } => {
                write!(f, "nesting exceeds the depth limit of {max}")
            }
            MsgpackError::Reserved => write!(f, "reserved head byte 0xc1"),
            MsgpackError::ExtNotAllowed => write!(f, "ext types are not allowed"),
            MsgpackError::InvalidUtf8 => write!(f, "str payload is not valid UTF-8"),
            MsgpackError::IntOutOfRange { found } => {
                write!(f, "integer {found} does not fit in i64")
            }
        }
    }
}

/// Errors raised by the type/encoding layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TypeError {
    /// Stored key bytes are corrupt.
    #[error("corrupt key encoding: {0}")]
    KeyCorrupt(KeyCorruption),
    /// Stored row bytes are corrupt.
    #[error("corrupt row encoding: {0}")]
    RowCorrupt(RowCorruption),
    /// Wire bytes are not well-formed MessagePack (or break a limit).
    #[error("malformed messagepack: {0}")]
    Msgpack(MsgpackError),
    /// The type cannot be a key component.
    #[error("{kind} values cannot be used in a key")]
    NotKeyable {
        /// The unsupported type's name.
        kind: &'static str,
    },
    /// A wire value's MessagePack type does not match the schema type.
    #[error("expected {expected} on the wire, found {found}")]
    WrongWireType {
        /// The schema type that was expected.
        expected: TypeKind,
        /// The MessagePack family actually found.
        found: &'static str,
    },
    /// A uuid string is not in the canonical hyphenated form.
    #[error("invalid uuid string")]
    BadUuid,
    /// A row has more columns than [`MAX_COLUMNS`].
    #[error("row has {count} columns, the maximum is {max}")]
    TooManyColumns {
        /// The offending column count.
        count: usize,
        /// The enforced limit.
        max: usize,
    },
    /// A single value exceeds the encodable size.
    #[error("value of {len} bytes exceeds the encodable size")]
    ValueTooLarge {
        /// The offending byte length.
        len: usize,
    },
}

impl CategorizedError for TypeError {
    fn category(&self) -> ErrorCategory {
        match self {
            // Stored bytes failing to decode is corruption — surfaced, never
            // served (SPEC §9).
            TypeError::KeyCorrupt(_) | TypeError::RowCorrupt(_) => ErrorCategory::Corruption,
            // Wire/user input problems are validation failures.
            TypeError::Msgpack(_)
            | TypeError::NotKeyable { .. }
            | TypeError::WrongWireType { .. }
            | TypeError::BadUuid => ErrorCategory::Validation,
            TypeError::TooManyColumns { .. } | TypeError::ValueTooLarge { .. } => {
                ErrorCategory::ResourceLimit
            }
        }
    }
}

/// Result alias for type operations.
pub type Result<T> = std::result::Result<T, TypeError>;
