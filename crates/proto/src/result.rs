//! Result encoding (`SPEC.md` §5.6) and the keyset cursor-token envelope.
//!
//! Every response is a map `{v, ok, columns, rows, cursor, applied,
//! affected}`; errors are `{v, ok:false, code, error}` with `code` from the
//! `SPEC.md` §9 category list. Row cells reuse the `types` wire mapping
//! (uuid as canonical string, timestamp as int, json spliced verbatim).

use common::{crc32c, ErrorCategory};
use types::Value;

use crate::wire::{write_array_header, write_bin, write_int, write_map_header, write_str};
use crate::{ProtoError, Result, PROTOCOL_VERSION};

/// One query result, ready to encode (`SPEC.md` §5.6).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryResult {
    /// Output column names, in row order.
    pub columns: Vec<String>,
    /// The rows; each cell is a wire value.
    pub rows: Vec<Vec<Value>>,
    /// A continuation token when more pages remain.
    pub cursor: Option<Vec<u8>>,
    /// Whether a guarded/conditional write applied.
    pub applied: Option<bool>,
    /// Rows changed by a write.
    pub affected: Option<u64>,
}

/// Encode a successful result:
/// `{v, ok:true, columns, rows, cursor, applied, affected}`. The optional
/// fields are explicit `null`s, matching the §5.6 shape.
pub fn encode_result(result: &QueryResult) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_map_header(&mut out, 7);
    write_str(&mut out, "v");
    write_int(&mut out, PROTOCOL_VERSION);
    write_str(&mut out, "ok");
    out.push(0xC3); // true
    write_str(&mut out, "columns");
    write_array_header(&mut out, result.columns.len());
    for name in &result.columns {
        write_str(&mut out, name);
    }
    write_str(&mut out, "rows");
    write_array_header(&mut out, result.rows.len());
    for row in &result.rows {
        write_array_header(&mut out, row.len());
        for cell in row {
            // The cell mapping (uuid/timestamp/json handling) lives in
            // `types`; the encoded value is spliced in verbatim.
            out.extend_from_slice(&types::encode_value(cell)?);
        }
    }
    write_str(&mut out, "cursor");
    match &result.cursor {
        Some(token) => write_bin(&mut out, token),
        None => out.push(0xC0),
    }
    write_str(&mut out, "applied");
    match result.applied {
        Some(true) => out.push(0xC3),
        Some(false) => out.push(0xC2),
        None => out.push(0xC0),
    }
    write_str(&mut out, "affected");
    match result.affected {
        Some(n) => write_int(&mut out, i64::try_from(n).unwrap_or(i64::MAX)),
        None => out.push(0xC0),
    }
    Ok(out)
}

/// Encode a typed error result: `{v, ok:false, code, error}` with `code`
/// being the stable `SPEC.md` §9 category identifier.
pub fn encode_error_result(category: ErrorCategory, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    write_map_header(&mut out, 4);
    write_str(&mut out, "v");
    write_int(&mut out, PROTOCOL_VERSION);
    write_str(&mut out, "ok");
    out.push(0xC2); // false
    write_str(&mut out, "code");
    write_str(&mut out, category.as_str());
    write_str(&mut out, "error");
    write_str(&mut out, message);
    out
}

/// The cursor-token format version.
const CURSOR_VERSION: u8 = 1;

/// Wrap an executor keyset payload in the opaque token envelope:
/// `[version][crc32c(payload) BE][payload]`. The payload's contents are the
/// executor's business (Phase 9); the envelope makes a mangled or truncated
/// token a clean `Validation` error instead of a nonsense seek.
pub fn encode_cursor_token(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(CURSOR_VERSION);
    out.extend_from_slice(&crc32c(payload).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Unwrap a cursor token, verifying version and checksum.
///
/// # Examples
///
/// ```
/// let token = proto::encode_cursor_token(b"position");
/// assert_eq!(proto::decode_cursor_token(&token).unwrap(), b"position");
/// assert!(proto::decode_cursor_token(&token[..4]).is_err());
/// ```
pub fn decode_cursor_token(token: &[u8]) -> Result<Vec<u8>> {
    let (header, payload) = match token.split_at_checked(5) {
        Some(parts) => parts,
        None => return Err(ProtoError::BadCursorToken),
    };
    if header[0] != CURSOR_VERSION {
        return Err(ProtoError::BadCursorToken);
    }
    let mut crc = [0u8; 4];
    crc.copy_from_slice(&header[1..5]);
    if u32::from_be_bytes(crc) != crc32c(payload) {
        return Err(ProtoError::BadCursorToken);
    }
    Ok(payload.to_vec())
}
