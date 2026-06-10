//! The row encoding: a compact, self-describing sequence of column values.
//!
//! Distinct from the key encoding — rows never need to be order-preserving,
//! only to round-trip exactly. Layout: a `u16` little-endian column count,
//! then per value one tag byte and a payload; variable-length payloads
//! (`text` / `blob` / `json`) carry a `u32` little-endian length prefix.

use crate::value::Value;
use crate::{Result, RowCorruption, TypeError};

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_I64: u8 = 2;
const TAG_F64: u8 = 3;
const TAG_TEXT: u8 = 4;
const TAG_BLOB: u8 = 5;
const TAG_UUID: u8 = 6;
const TAG_JSON: u8 = 7;
const TAG_TIMESTAMP: u8 = 8;

/// The most columns a row may carry (fits the `u16` count with headroom).
pub const MAX_COLUMNS: usize = 4096;

/// Encode a row (one value per column, in schema column order).
///
/// Returns [`TypeError::TooManyColumns`] above [`MAX_COLUMNS`].
///
/// # Examples
///
/// ```
/// use types::{decode_row, encode_row, Value};
///
/// let row = vec![Value::I64(7), Value::Null, Value::Text("hi".into())];
/// let bytes = encode_row(&row).unwrap();
/// assert_eq!(decode_row(&bytes).unwrap(), row);
/// ```
pub fn encode_row(values: &[Value]) -> Result<Vec<u8>> {
    if values.len() > MAX_COLUMNS {
        return Err(TypeError::TooManyColumns {
            count: values.len(),
            max: MAX_COLUMNS,
        });
    }
    let mut out = Vec::new();
    out.extend_from_slice(&(values.len() as u16).to_le_bytes());
    for value in values {
        match value {
            Value::Null => out.push(TAG_NULL),
            Value::Bool(b) => {
                out.push(TAG_BOOL);
                out.push(u8::from(*b));
            }
            Value::I64(v) => {
                out.push(TAG_I64);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Value::F64(v) => {
                out.push(TAG_F64);
                out.extend_from_slice(&v.to_bits().to_le_bytes());
            }
            Value::Text(s) => {
                out.push(TAG_TEXT);
                push_var(&mut out, s.as_bytes())?;
            }
            Value::Blob(b) => {
                out.push(TAG_BLOB);
                push_var(&mut out, b)?;
            }
            Value::Uuid(u) => {
                out.push(TAG_UUID);
                out.extend_from_slice(u);
            }
            Value::Json(b) => {
                out.push(TAG_JSON);
                push_var(&mut out, b)?;
            }
            Value::Timestamp(v) => {
                out.push(TAG_TIMESTAMP);
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    Ok(out)
}

fn push_var(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    let len =
        u32::try_from(bytes.len()).map_err(|_| TypeError::ValueTooLarge { len: bytes.len() })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// Decode a row produced by [`encode_row`].
///
/// The input is stored bytes, so every malformation — truncation, a bad tag,
/// a length prefix past the end, trailing garbage — is a typed
/// [`TypeError::RowCorrupt`], never a panic.
pub fn decode_row(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut r = Reader { rest: bytes };
    let count = usize::from(u16::from_le_bytes(r.take_array::<2>()?));
    let mut values = Vec::with_capacity(count.min(MAX_COLUMNS));
    for _ in 0..count {
        values.push(r.value()?);
    }
    if !r.rest.is_empty() {
        return Err(corrupt(RowCorruption::TrailingBytes {
            count: r.rest.len(),
        }));
    }
    Ok(values)
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.rest.len() < n {
            return Err(corrupt(RowCorruption::Truncated));
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

    fn take_var(&mut self) -> Result<&'a [u8]> {
        let len = u32::from_le_bytes(self.take_array::<4>()?) as usize;
        self.take(len)
    }

    fn value(&mut self) -> Result<Value> {
        let tag = self.take_array::<1>()?[0];
        match tag {
            TAG_NULL => Ok(Value::Null),
            TAG_BOOL => match self.take_array::<1>()?[0] {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                byte => Err(corrupt(RowCorruption::BadBool { byte })),
            },
            TAG_I64 => Ok(Value::I64(i64::from_le_bytes(self.take_array::<8>()?))),
            TAG_F64 => Ok(Value::F64(f64::from_bits(u64::from_le_bytes(
                self.take_array::<8>()?,
            )))),
            TAG_TEXT => {
                let bytes = self.take_var()?.to_vec();
                let text =
                    String::from_utf8(bytes).map_err(|_| corrupt(RowCorruption::InvalidUtf8))?;
                Ok(Value::Text(text))
            }
            TAG_BLOB => Ok(Value::Blob(self.take_var()?.to_vec())),
            TAG_UUID => Ok(Value::Uuid(self.take_array::<16>()?)),
            TAG_JSON => Ok(Value::Json(self.take_var()?.to_vec())),
            TAG_TIMESTAMP => Ok(Value::Timestamp(i64::from_le_bytes(
                self.take_array::<8>()?,
            ))),
            _ => Err(corrupt(RowCorruption::BadTag { tag })),
        }
    }
}

fn corrupt(kind: RowCorruption) -> TypeError {
    TypeError::RowCorrupt(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_and_negative_zero_round_trip_bit_exact() {
        let row = vec![Value::F64(f64::NAN), Value::F64(-0.0)];
        let back = decode_row(&encode_row(&row).unwrap()).unwrap();
        match (&back[0], &back[1]) {
            (Value::F64(a), Value::F64(b)) => {
                assert!(a.is_nan());
                assert_eq!(b.to_bits(), (-0.0f64).to_bits());
            }
            other => panic!("wrong variants: {other:?}"),
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode_row(&[Value::I64(1)]).unwrap();
        bytes.push(0xAA);
        assert!(matches!(
            decode_row(&bytes),
            Err(TypeError::RowCorrupt(RowCorruption::TrailingBytes { .. }))
        ));
    }
}
