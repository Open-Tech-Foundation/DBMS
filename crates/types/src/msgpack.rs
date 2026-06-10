//! A minimal, hardened MessagePack codec (in-house per `DECISIONS.md` D4/D11).
//!
//! Covers what the engine needs: the wire mapping for [`Value`]s and the
//! well-formedness check for `json` documents (which are *stored as* their
//! MessagePack bytes, opaque in v1). Decoding is defensive: depth-limited,
//! bounds-checked, rejects the reserved byte `0xC1` and (in documents) ext
//! types, and never panics on hostile input. Phase 8's AST decoding builds on
//! this module.

use crate::uuid::{uuid_from_str, uuid_to_string};
use crate::value::{TypeKind, Value};
use crate::{MsgpackError, Result, TypeError};

/// The deepest nesting allowed in a `json` document (arrays/maps).
pub const MAX_JSON_DEPTH: usize = 64;

// --- encoding --------------------------------------------------------------

/// Encode one [`Value`] in its MessagePack wire form.
///
/// - `uuid` encodes as its canonical hyphenated string (`SPEC.md` §3:
///   "canonical string on I/O").
/// - `timestamp` encodes as its epoch-microseconds integer.
/// - `json` is spliced in verbatim — the document already *is* MessagePack.
///
/// Decoding is schema-directed: see [`decode_value`].
pub fn encode_value(value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match value {
        Value::Null => out.push(0xC0),
        Value::Bool(b) => out.push(if *b { 0xC3 } else { 0xC2 }),
        Value::I64(v) => write_int(&mut out, *v),
        Value::Timestamp(v) => write_int(&mut out, *v),
        Value::F64(v) => {
            out.push(0xCB);
            out.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        Value::Text(s) => write_str(&mut out, s)?,
        Value::Uuid(u) => write_str(&mut out, &uuid_to_string(u))?,
        Value::Blob(b) => write_bin(&mut out, b)?,
        Value::Json(doc) => out.extend_from_slice(doc),
    }
    Ok(out)
}

/// Write `v` in the most compact MessagePack int form.
fn write_int(out: &mut Vec<u8>, v: i64) {
    match v {
        0.. => match u64::try_from(v).unwrap_or(0) {
            n @ 0..=0x7F => out.push(n as u8),
            n @ ..=0xFF => {
                out.push(0xCC);
                out.push(n as u8);
            }
            n @ ..=0xFFFF => {
                out.push(0xCD);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n @ ..=0xFFFF_FFFF => {
                out.push(0xCE);
                out.extend_from_slice(&(n as u32).to_be_bytes());
            }
            n => {
                out.push(0xCF);
                out.extend_from_slice(&n.to_be_bytes());
            }
        },
        -32..=-1 => out.push(v as u8),
        -128..=-33 => {
            out.push(0xD0);
            out.push(v as u8);
        }
        -32768..=-129 => {
            out.push(0xD1);
            out.extend_from_slice(&(v as i16).to_be_bytes());
        }
        -2_147_483_648..=-32769 => {
            out.push(0xD2);
            out.extend_from_slice(&(v as i32).to_be_bytes());
        }
        _ => {
            out.push(0xD3);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    match bytes.len() {
        n @ ..=0x1F => out.push(0xA0 | n as u8),
        n @ ..=0xFF => {
            out.push(0xD9);
            out.push(n as u8);
        }
        n @ ..=0xFFFF => {
            out.push(0xDA);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            let len = u32::try_from(n).map_err(|_| TypeError::ValueTooLarge { len: n })?;
            out.push(0xDB);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn write_bin(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    match bytes.len() {
        n @ ..=0xFF => {
            out.push(0xC4);
            out.push(n as u8);
        }
        n @ ..=0xFFFF => {
            out.push(0xC5);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            let len = u32::try_from(n).map_err(|_| TypeError::ValueTooLarge { len: n })?;
            out.push(0xC6);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    out.extend_from_slice(bytes);
    Ok(())
}

// --- decoding --------------------------------------------------------------

/// Decode one wire value as the schema type `kind` (strict, no coercion
/// beyond the wire mapping itself). `nil` decodes to [`Value::Null`] for any
/// kind — nullability is a constraint checked elsewhere.
///
/// The whole input must be exactly one value; trailing bytes are rejected.
///
/// # Examples
///
/// ```
/// use types::{decode_value, encode_value, TypeKind, Value};
///
/// let v = Value::Timestamp(1_700_000_000_000_000);
/// let wire = encode_value(&v).unwrap();
/// assert_eq!(decode_value(&wire, TypeKind::Timestamp).unwrap(), v);
/// ```
pub fn decode_value(bytes: &[u8], kind: TypeKind) -> Result<Value> {
    let mut r = Reader::new(bytes);
    let value = r.value(kind)?;
    r.finish()?;
    Ok(value)
}

/// Check that `bytes` are exactly one **well-formed** MessagePack document:
/// no truncation, no trailing bytes, no reserved byte, no ext types, nesting
/// within [`MAX_JSON_DEPTH`]. This is the `json` ingestion gate — documents
/// are stored verbatim once they pass.
pub fn validate_json(bytes: &[u8]) -> Result<()> {
    let mut r = Reader::new(bytes);
    r.skip_value(0)?;
    r.finish()
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { rest: bytes }
    }

    fn finish(&self) -> Result<()> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(bad(MsgpackError::TrailingBytes {
                count: self.rest.len(),
            }))
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.rest.len() < n {
            return Err(bad(MsgpackError::Truncated));
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Ok(head)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take_array::<1>()?[0])
    }

    /// Decode one value, directed by the expected schema `kind`.
    fn value(&mut self, kind: TypeKind) -> Result<Value> {
        // `json` accepts any well-formed document and stores it verbatim.
        if kind == TypeKind::Json {
            let before = self.rest;
            if before.first() == Some(&0xC0) {
                self.take(1)?;
                return Ok(Value::Null);
            }
            self.skip_value(0)?;
            let len = before.len() - self.rest.len();
            let doc = before
                .get(..len)
                .ok_or_else(|| bad(MsgpackError::Truncated))?;
            return Ok(Value::Json(doc.to_vec()));
        }

        let head = self.byte()?;
        if head == 0xC0 {
            return Ok(Value::Null);
        }
        match kind {
            TypeKind::Bool => match head {
                0xC2 => Ok(Value::Bool(false)),
                0xC3 => Ok(Value::Bool(true)),
                _ => Err(self.wrong(kind, head)),
            },
            TypeKind::I64 => Ok(Value::I64(self.int_payload(kind, head)?)),
            TypeKind::Timestamp => Ok(Value::Timestamp(self.int_payload(kind, head)?)),
            TypeKind::F64 => match head {
                0xCA => {
                    let bits = u32::from_be_bytes(self.take_array::<4>()?);
                    Ok(Value::F64(f64::from(f32::from_bits(bits))))
                }
                0xCB => {
                    let bits = u64::from_be_bytes(self.take_array::<8>()?);
                    Ok(Value::F64(f64::from_bits(bits)))
                }
                _ => Err(self.wrong(kind, head)),
            },
            TypeKind::Text => Ok(Value::Text(self.str_payload(kind, head)?)),
            TypeKind::Uuid => {
                let s = self.str_payload(kind, head)?;
                Ok(Value::Uuid(uuid_from_str(&s)?))
            }
            TypeKind::Blob => {
                let len = match head {
                    0xC4 => usize::from(self.byte()?),
                    0xC5 => usize::from(u16::from_be_bytes(self.take_array::<2>()?)),
                    0xC6 => u32::from_be_bytes(self.take_array::<4>()?) as usize,
                    _ => return Err(self.wrong(kind, head)),
                };
                Ok(Value::Blob(self.take(len)?.to_vec()))
            }
            // Handled by the early return above; an error arm (not a panic
            // path) keeps the library panic-free under any refactor.
            TypeKind::Json => Err(self.wrong(kind, head)),
        }
    }

    /// Any MessagePack int family, range-checked into `i64`.
    fn int_payload(&mut self, kind: TypeKind, head: u8) -> Result<i64> {
        match head {
            0x00..=0x7F => Ok(i64::from(head)),
            0xE0..=0xFF => Ok(i64::from(head as i8)),
            0xCC => Ok(i64::from(self.byte()?)),
            0xCD => Ok(i64::from(u16::from_be_bytes(self.take_array::<2>()?))),
            0xCE => Ok(i64::from(u32::from_be_bytes(self.take_array::<4>()?))),
            0xCF => {
                let n = u64::from_be_bytes(self.take_array::<8>()?);
                i64::try_from(n).map_err(|_| bad(MsgpackError::IntOutOfRange { found: n }))
            }
            0xD0 => Ok(i64::from(self.take_array::<1>()?[0] as i8)),
            0xD1 => Ok(i64::from(i16::from_be_bytes(self.take_array::<2>()?))),
            0xD2 => Ok(i64::from(i32::from_be_bytes(self.take_array::<4>()?))),
            0xD3 => Ok(i64::from_be_bytes(self.take_array::<8>()?)),
            _ => Err(self.wrong(kind, head)),
        }
    }

    fn str_payload(&mut self, kind: TypeKind, head: u8) -> Result<String> {
        let len = match head {
            0xA0..=0xBF => usize::from(head & 0x1F),
            0xD9 => usize::from(self.byte()?),
            0xDA => usize::from(u16::from_be_bytes(self.take_array::<2>()?)),
            0xDB => u32::from_be_bytes(self.take_array::<4>()?) as usize,
            _ => return Err(self.wrong(kind, head)),
        };
        let bytes = self.take(len)?.to_vec();
        String::from_utf8(bytes).map_err(|_| bad(MsgpackError::InvalidUtf8))
    }

    /// Walk (and discard) one value of any shape, enforcing the depth limit
    /// and rejecting reserved/ext bytes. The well-formedness core.
    fn skip_value(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_JSON_DEPTH {
            return Err(bad(MsgpackError::DepthExceeded {
                max: MAX_JSON_DEPTH,
            }));
        }
        let head = self.byte()?;
        let (skip_bytes, items) = match head {
            // Scalars with fixed payloads.
            0x00..=0x7F | 0xE0..=0xFF | 0xC0 | 0xC2 | 0xC3 => (0, 0),
            0xCC | 0xD0 => (1, 0),
            0xCD | 0xD1 => (2, 0),
            0xCE | 0xD2 | 0xCA => (4, 0),
            0xCF | 0xD3 | 0xCB => (8, 0),
            // Strings and bins: length prefix then payload.
            0xA0..=0xBF => (usize::from(head & 0x1F), 0),
            0xD9 | 0xC4 => (usize::from(self.byte()?), 0),
            0xDA | 0xC5 => (usize::from(u16::from_be_bytes(self.take_array::<2>()?)), 0),
            0xDB | 0xC6 => (u32::from_be_bytes(self.take_array::<4>()?) as usize, 0),
            // Containers: item counts (map entries are key+value pairs).
            0x80..=0x8F => (0, usize::from(head & 0x0F) * 2),
            0x90..=0x9F => (0, usize::from(head & 0x0F)),
            0xDC => (0, usize::from(u16::from_be_bytes(self.take_array::<2>()?))),
            0xDD => (0, u32::from_be_bytes(self.take_array::<4>()?) as usize),
            0xDE => (
                0,
                usize::from(u16::from_be_bytes(self.take_array::<2>()?)) * 2,
            ),
            0xDF => {
                let n = u32::from_be_bytes(self.take_array::<4>()?) as usize;
                (0, n.saturating_mul(2))
            }
            // Ext types: not allowed in opaque v1 documents.
            0xC7..=0xC9 | 0xD4..=0xD8 => return Err(bad(MsgpackError::ExtNotAllowed)),
            0xC1 => return Err(bad(MsgpackError::Reserved)),
        };
        if skip_bytes > 0 {
            self.take(skip_bytes)?;
        }
        for _ in 0..items {
            self.skip_value(depth + 1)?;
        }
        Ok(())
    }

    fn wrong(&self, expected: TypeKind, head: u8) -> TypeError {
        TypeError::WrongWireType {
            expected,
            found: head_name(head),
        }
    }
}

/// A human-readable name for a MessagePack head byte's family.
fn head_name(head: u8) -> &'static str {
    match head {
        0x00..=0x7F | 0xE0..=0xFF | 0xCC..=0xCF | 0xD0..=0xD3 => "int",
        0x80..=0x8F | 0xDE | 0xDF => "map",
        0x90..=0x9F | 0xDC | 0xDD => "array",
        0xA0..=0xBF | 0xD9..=0xDB => "str",
        0xC0 => "nil",
        0xC2 | 0xC3 => "bool",
        0xC4..=0xC6 => "bin",
        0xC7..=0xC9 | 0xD4..=0xD8 => "ext",
        0xCA | 0xCB => "float",
        0xC1 => "reserved",
    }
}

fn bad(err: MsgpackError) -> TypeError {
    TypeError::Msgpack(err)
}
