//! The hardened wire layer: MessagePack bytes ⇄ a bounded [`Doc`] tree.
//!
//! Every protocol message is decoded through [`decode_doc`] first, which
//! enforces [`DecodeLimits`] (message size, nesting depth, node count)
//! **before** allocating, walks with an explicit depth counter, and rejects
//! the reserved byte `0xC1`, ext types, non-string map keys, duplicate map
//! keys, and trailing bytes (`ARCHITECTURE.md` §6 "decoder hardening"). The
//! AST layer then maps the already-safe `Doc` tree onto query nodes.

use crate::{ProtoError, Result};

/// Decoder resource limits (`SPEC.md` §8: configurable caps; clean typed
/// errors on breach).
#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    /// Maximum total message size in bytes.
    pub max_bytes: usize,
    /// Maximum container nesting depth.
    pub max_depth: usize,
    /// Maximum total number of nodes (scalars + containers).
    pub max_nodes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        DecodeLimits {
            max_bytes: 1 << 20, // 1 MiB
            max_depth: 64,      // matches types::MAX_JSON_DEPTH
            max_nodes: 100_000,
        }
    }
}

/// One decoded MessagePack value, shape-checked and within limits.
///
/// Map entries preserve wire order; keys are guaranteed unique strings.
#[derive(Debug, Clone, PartialEq)]
pub enum Doc {
    /// `nil`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// Any int family, range-checked into `i64`.
    Int(i64),
    /// `float32` (widened) or `float64`.
    Float(f64),
    /// A UTF-8 string.
    Str(String),
    /// A bin payload.
    Bin(Vec<u8>),
    /// An array.
    Array(Vec<Doc>),
    /// A map with unique string keys, in wire order.
    Map(Vec<(String, Doc)>),
}

impl Doc {
    /// A human-readable name for the value's shape (used in errors).
    pub fn shape(&self) -> &'static str {
        match self {
            Doc::Null => "null",
            Doc::Bool(_) => "bool",
            Doc::Int(_) => "int",
            Doc::Float(_) => "float",
            Doc::Str(_) => "string",
            Doc::Bin(_) => "bin",
            Doc::Array(_) => "array",
            Doc::Map(_) => "map",
        }
    }

    /// Look up a key in a map `Doc`; `None` for absent keys or non-maps.
    pub fn get(&self, key: &str) -> Option<&Doc> {
        match self {
            Doc::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Decode exactly one MessagePack value under `limits`.
pub fn decode_doc(bytes: &[u8], limits: &DecodeLimits) -> Result<Doc> {
    if bytes.len() > limits.max_bytes {
        return Err(ProtoError::MessageTooLarge {
            len: bytes.len(),
            max: limits.max_bytes,
        });
    }
    let mut r = Reader {
        rest: bytes,
        nodes_left: limits.max_nodes,
        max_depth: limits.max_depth,
    };
    let doc = r.value(0)?;
    if !r.rest.is_empty() {
        return Err(ProtoError::TrailingBytes {
            count: r.rest.len(),
        });
    }
    Ok(doc)
}

struct Reader<'a> {
    rest: &'a [u8],
    nodes_left: usize,
    max_depth: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.rest.len() < n {
            return Err(ProtoError::Truncated);
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

    /// Charge one node against the budget.
    fn node(&mut self) -> Result<()> {
        match self.nodes_left.checked_sub(1) {
            Some(left) => {
                self.nodes_left = left;
                Ok(())
            }
            None => Err(ProtoError::TooManyNodes),
        }
    }

    /// Check a claimed container item count against what could possibly fit
    /// in the remaining bytes (≥ 1 byte per item) *and* the node budget —
    /// before any allocation.
    fn check_count(&self, n: usize) -> Result<()> {
        if n > self.rest.len() {
            return Err(ProtoError::Truncated);
        }
        if n > self.nodes_left {
            return Err(ProtoError::TooManyNodes);
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Doc> {
        if depth > self.max_depth {
            return Err(ProtoError::TooDeep {
                max: self.max_depth,
            });
        }
        self.node()?;
        let head = self.byte()?;
        let doc = match head {
            // Ints (all families, range-checked into i64).
            0x00..=0x7F => Doc::Int(i64::from(head)),
            0xE0..=0xFF => Doc::Int(i64::from(head as i8)),
            0xCC => Doc::Int(i64::from(self.byte()?)),
            0xCD => Doc::Int(i64::from(u16::from_be_bytes(self.take_array::<2>()?))),
            0xCE => Doc::Int(i64::from(u32::from_be_bytes(self.take_array::<4>()?))),
            0xCF => {
                let n = u64::from_be_bytes(self.take_array::<8>()?);
                Doc::Int(i64::try_from(n).map_err(|_| ProtoError::IntOutOfRange { found: n })?)
            }
            0xD0 => Doc::Int(i64::from(self.take_array::<1>()?[0] as i8)),
            0xD1 => Doc::Int(i64::from(i16::from_be_bytes(self.take_array::<2>()?))),
            0xD2 => Doc::Int(i64::from(i32::from_be_bytes(self.take_array::<4>()?))),
            0xD3 => Doc::Int(i64::from_be_bytes(self.take_array::<8>()?)),
            // Other scalars.
            0xC0 => Doc::Null,
            0xC2 => Doc::Bool(false),
            0xC3 => Doc::Bool(true),
            0xCA => Doc::Float(f64::from(f32::from_bits(u32::from_be_bytes(
                self.take_array::<4>()?,
            )))),
            0xCB => Doc::Float(f64::from_bits(u64::from_be_bytes(self.take_array::<8>()?))),
            // Strings.
            0xA0..=0xBF => self.str_body(usize::from(head & 0x1F))?,
            0xD9 => {
                let n = usize::from(self.byte()?);
                self.str_body(n)?
            }
            0xDA => {
                let n = usize::from(u16::from_be_bytes(self.take_array::<2>()?));
                self.str_body(n)?
            }
            0xDB => {
                let n = u32::from_be_bytes(self.take_array::<4>()?) as usize;
                self.str_body(n)?
            }
            // Bins.
            0xC4 => {
                let n = usize::from(self.byte()?);
                Doc::Bin(self.take(n)?.to_vec())
            }
            0xC5 => {
                let n = usize::from(u16::from_be_bytes(self.take_array::<2>()?));
                Doc::Bin(self.take(n)?.to_vec())
            }
            0xC6 => {
                let n = u32::from_be_bytes(self.take_array::<4>()?) as usize;
                Doc::Bin(self.take(n)?.to_vec())
            }
            // Arrays.
            0x90..=0x9F => self.array_body(usize::from(head & 0x0F), depth)?,
            0xDC => {
                let n = usize::from(u16::from_be_bytes(self.take_array::<2>()?));
                self.array_body(n, depth)?
            }
            0xDD => {
                let n = u32::from_be_bytes(self.take_array::<4>()?) as usize;
                self.array_body(n, depth)?
            }
            // Maps.
            0x80..=0x8F => self.map_body(usize::from(head & 0x0F), depth)?,
            0xDE => {
                let n = usize::from(u16::from_be_bytes(self.take_array::<2>()?));
                self.map_body(n, depth)?
            }
            0xDF => {
                let n = u32::from_be_bytes(self.take_array::<4>()?) as usize;
                self.map_body(n, depth)?
            }
            // Ext types are not part of the protocol; 0xC1 is reserved.
            0xC7..=0xC9 | 0xD4..=0xD8 => return Err(ProtoError::ExtNotAllowed),
            0xC1 => return Err(ProtoError::ReservedByte),
        };
        Ok(doc)
    }

    fn str_body(&mut self, len: usize) -> Result<Doc> {
        let bytes = self.take(len)?.to_vec();
        let s = String::from_utf8(bytes).map_err(|_| ProtoError::InvalidUtf8)?;
        Ok(Doc::Str(s))
    }

    fn array_body(&mut self, n: usize, depth: usize) -> Result<Doc> {
        self.check_count(n)?;
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            items.push(self.value(depth + 1)?);
        }
        Ok(Doc::Array(items))
    }

    fn map_body(&mut self, n: usize, depth: usize) -> Result<Doc> {
        // A map entry is a key node and a value node.
        self.check_count(n.saturating_mul(2))?;
        let mut entries: Vec<(String, Doc)> = Vec::with_capacity(n);
        for _ in 0..n {
            self.node()?;
            let key = match self.value_is_str()? {
                Some(key) => key,
                None => return Err(ProtoError::NonStringKey),
            };
            if entries.iter().any(|(k, _)| *k == key) {
                return Err(ProtoError::DuplicateKey { key });
            }
            let value = self.value(depth + 1)?;
            entries.push((key, value));
        }
        Ok(Doc::Map(entries))
    }

    /// Read one value that must be a string (a map key); `None` if the head
    /// byte is any other shape.
    fn value_is_str(&mut self) -> Result<Option<String>> {
        let head = self.byte()?;
        let len = match head {
            0xA0..=0xBF => usize::from(head & 0x1F),
            0xD9 => usize::from(self.byte()?),
            0xDA => usize::from(u16::from_be_bytes(self.take_array::<2>()?)),
            0xDB => u32::from_be_bytes(self.take_array::<4>()?) as usize,
            _ => return Ok(None),
        };
        match self.str_body(len)? {
            Doc::Str(s) => Ok(Some(s)),
            // str_body only returns Doc::Str; an error arm (not a panic
            // path) keeps the library panic-free under any refactor.
            _ => Ok(None),
        }
    }
}

// --- encoding ----------------------------------------------------------------

/// Encode a [`Doc`] in its MessagePack wire form (the inverse of
/// [`decode_doc`]; round-trips exactly).
pub fn encode_doc(doc: &Doc) -> Vec<u8> {
    let mut out = Vec::new();
    write_doc(&mut out, doc);
    out
}

pub(crate) fn write_doc(out: &mut Vec<u8>, doc: &Doc) {
    match doc {
        Doc::Null => out.push(0xC0),
        Doc::Bool(b) => out.push(if *b { 0xC3 } else { 0xC2 }),
        Doc::Int(v) => write_int(out, *v),
        Doc::Float(v) => {
            out.push(0xCB);
            out.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        Doc::Str(s) => write_str(out, s),
        Doc::Bin(b) => write_bin(out, b),
        Doc::Array(items) => {
            write_array_header(out, items.len());
            for item in items {
                write_doc(out, item);
            }
        }
        Doc::Map(entries) => {
            write_map_header(out, entries.len());
            for (key, value) in entries {
                write_str(out, key);
                write_doc(out, value);
            }
        }
    }
}

/// Write `v` in the most compact MessagePack int form.
pub(crate) fn write_int(out: &mut Vec<u8>, v: i64) {
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

pub(crate) fn write_str(out: &mut Vec<u8>, s: &str) {
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
            // Strings beyond u32 are unreachable through the bounded protocol
            // surface; saturate rather than panic.
            let len = u32::try_from(n).unwrap_or(u32::MAX);
            out.push(0xDB);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    out.extend_from_slice(bytes);
}

pub(crate) fn write_bin(out: &mut Vec<u8>, bytes: &[u8]) {
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
            let len = u32::try_from(n).unwrap_or(u32::MAX);
            out.push(0xC6);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    out.extend_from_slice(bytes);
}

pub(crate) fn write_array_header(out: &mut Vec<u8>, n: usize) {
    match n {
        0..=0x0F => out.push(0x90 | n as u8),
        0x10..=0xFFFF => {
            out.push(0xDC);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            out.push(0xDD);
            out.extend_from_slice(&(u32::try_from(n).unwrap_or(u32::MAX)).to_be_bytes());
        }
    }
}

pub(crate) fn write_map_header(out: &mut Vec<u8>, n: usize) {
    match n {
        0..=0x0F => out.push(0x80 | n as u8),
        0x10..=0xFFFF => {
            out.push(0xDE);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            out.push(0xDF);
            out.extend_from_slice(&(u32::try_from(n).unwrap_or(u32::MAX)).to_be_bytes());
        }
    }
}
