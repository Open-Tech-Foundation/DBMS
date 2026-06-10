//! The order-preserving key encoding.
//!
//! For any two keyable values (and any two composite keys),
//! `bytewise_cmp(encode(a), encode(b)) == logical_cmp(a, b)`. Every encoded
//! value is **prefix-free**, so composite keys are plain concatenation and the
//! encoding decodes back exactly — Phase 7 relies on that to recover PK
//! suffixes from index entries.
//!
//! Layout per value: one tag byte (fixing the cross-type rank, nulls first),
//! then a payload:
//!
//! | type | payload |
//! |---|---|
//! | `null` | none |
//! | `bool` | `0x00` / `0x01` |
//! | `i64`, `timestamp` | 8 bytes big-endian, sign bit flipped |
//! | `f64` | 8 bytes big-endian IEEE-754 total-order mapping |
//! | `text`, `blob` | bytes with `0x00` → `0x00 0xFF`, terminated `0x00 0x00` |
//! | `uuid` | 16 raw bytes |
//!
//! `json` is opaque in v1 and cannot be a key component.

use crate::value::{f64_from_total_key, f64_total_key, Value};
use crate::{KeyCorruption, Result, TypeError};

const TAG_NULL: u8 = 0x01;
const TAG_BOOL: u8 = 0x02;
const TAG_I64: u8 = 0x03;
const TAG_F64: u8 = 0x04;
const TAG_TEXT: u8 = 0x05;
const TAG_BLOB: u8 = 0x06;
const TAG_UUID: u8 = 0x07;
const TAG_TIMESTAMP: u8 = 0x08;

/// Encode a composite key (one or more components) into its order-preserving
/// byte form. A single-column key is a one-element slice.
///
/// Returns [`TypeError::NotKeyable`] if a component is `json` (opaque in v1).
///
/// # Examples
///
/// ```
/// use types::{encode_key, Value};
///
/// let lo = encode_key(&[Value::I64(-5)]).unwrap();
/// let hi = encode_key(&[Value::I64(3)]).unwrap();
/// assert!(lo < hi);
/// ```
pub fn encode_key(components: &[Value]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for value in components {
        encode_into(&mut out, value)?;
    }
    Ok(out)
}

fn encode_into(out: &mut Vec<u8>, value: &Value) -> Result<()> {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(b) => {
            out.push(TAG_BOOL);
            out.push(u8::from(*b));
        }
        Value::I64(v) => {
            out.push(TAG_I64);
            out.extend_from_slice(&flip_sign(*v).to_be_bytes());
        }
        Value::F64(v) => {
            out.push(TAG_F64);
            out.extend_from_slice(&f64_total_key(*v).to_be_bytes());
        }
        Value::Text(s) => {
            out.push(TAG_TEXT);
            escape_into(out, s.as_bytes());
        }
        Value::Blob(b) => {
            out.push(TAG_BLOB);
            escape_into(out, b);
        }
        Value::Uuid(u) => {
            out.push(TAG_UUID);
            out.extend_from_slice(u);
        }
        Value::Timestamp(v) => {
            out.push(TAG_TIMESTAMP);
            out.extend_from_slice(&flip_sign(*v).to_be_bytes());
        }
        Value::Json(_) => return Err(TypeError::NotKeyable { kind: "json" }),
    }
    Ok(())
}

/// Decode a key produced by [`encode_key`] back into its components.
///
/// The input is stored bytes, so every malformation is a typed
/// [`TypeError::KeyCorrupt`] — never a panic.
pub fn decode_key(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut components = Vec::new();
    let mut rest = bytes;
    while let Some((&tag, after_tag)) = rest.split_first() {
        let (value, after_value) = decode_one(tag, after_tag)?;
        components.push(value);
        rest = after_value;
    }
    Ok(components)
}

fn decode_one(tag: u8, rest: &[u8]) -> Result<(Value, &[u8])> {
    match tag {
        TAG_NULL => Ok((Value::Null, rest)),
        TAG_BOOL => {
            let (&byte, rest) = rest
                .split_first()
                .ok_or_else(|| corrupt(KeyCorruption::Truncated))?;
            match byte {
                0 => Ok((Value::Bool(false), rest)),
                1 => Ok((Value::Bool(true), rest)),
                _ => Err(corrupt(KeyCorruption::BadBool { byte })),
            }
        }
        TAG_I64 => {
            let (word, rest) = take_u64(rest)?;
            Ok((Value::I64(unflip_sign(word)), rest))
        }
        TAG_F64 => {
            let (word, rest) = take_u64(rest)?;
            Ok((Value::F64(f64_from_total_key(word)), rest))
        }
        TAG_TEXT => {
            let (bytes, rest) = unescape(rest)?;
            let text = String::from_utf8(bytes).map_err(|_| corrupt(KeyCorruption::InvalidUtf8))?;
            Ok((Value::Text(text), rest))
        }
        TAG_BLOB => {
            let (bytes, rest) = unescape(rest)?;
            Ok((Value::Blob(bytes), rest))
        }
        TAG_UUID => {
            if rest.len() < 16 {
                return Err(corrupt(KeyCorruption::Truncated));
            }
            let (head, rest) = rest.split_at(16);
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(head);
            Ok((Value::Uuid(uuid), rest))
        }
        TAG_TIMESTAMP => {
            let (word, rest) = take_u64(rest)?;
            Ok((Value::Timestamp(unflip_sign(word)), rest))
        }
        _ => Err(corrupt(KeyCorruption::BadTag { tag })),
    }
}

/// Map an `i64` to a `u64` whose unsigned (big-endian byte) order matches the
/// signed order: flip the sign bit.
fn flip_sign(v: i64) -> u64 {
    (v as u64) ^ (1 << 63)
}

fn unflip_sign(word: u64) -> i64 {
    (word ^ (1 << 63)) as i64
}

fn take_u64(rest: &[u8]) -> Result<(u64, &[u8])> {
    if rest.len() < 8 {
        return Err(corrupt(KeyCorruption::Truncated));
    }
    let (head, rest) = rest.split_at(8);
    let mut word = [0u8; 8];
    word.copy_from_slice(head);
    Ok((u64::from_be_bytes(word), rest))
}

/// Escape variable-length bytes so they are prefix-free yet order-preserving:
/// every `0x00` becomes `0x00 0xFF`, and the value ends with `0x00 0x00`.
/// A proper prefix then terminates (`0x00 0x00`) exactly where the longer
/// value continues with either an escaped zero (`0x00 0xFF`, larger) or any
/// other byte (`0x01..`, larger) — so prefixes sort first, matching the
/// logical bytewise order.
fn escape_into(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        out.push(b);
        if b == 0x00 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0x00, 0x00]);
}

fn unescape(mut rest: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    while let Some((&b, tail)) = rest.split_first() {
        if b != 0x00 {
            out.push(b);
            rest = tail;
            continue;
        }
        match tail.split_first() {
            Some((&0x00, after)) => return Ok((out, after)),
            Some((&0xFF, after)) => {
                out.push(0x00);
                rest = after;
            }
            Some((&escape, _)) => return Err(corrupt(KeyCorruption::BadEscape { escape })),
            None => return Err(corrupt(KeyCorruption::Truncated)),
        }
    }
    Err(corrupt(KeyCorruption::Truncated))
}

fn corrupt(kind: KeyCorruption) -> TypeError {
    TypeError::KeyCorrupt(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_sorts_before_extension() {
        let a = encode_key(&[Value::Text("ab".into())]).unwrap();
        let b = encode_key(&[Value::Text("ab\u{0}".into())]).unwrap();
        let c = encode_key(&[Value::Text("abc".into())]).unwrap();
        assert!(a < b && b < c);
    }

    #[test]
    fn composite_orders_component_wise() {
        let a = encode_key(&[Value::Text("a".into()), Value::I64(9)]).unwrap();
        let b = encode_key(&[Value::Text("ab".into()), Value::I64(0)]).unwrap();
        assert!(a < b, "shorter first component must dominate");
    }

    #[test]
    fn json_is_not_keyable() {
        let err = encode_key(&[Value::Json(vec![0xC0])]).unwrap_err();
        assert!(matches!(err, TypeError::NotKeyable { kind: "json" }));
    }
}
