//! The `Value` model (`SPEC.md` §3) and the engine's logical total order.

use std::cmp::Ordering;

/// The kind of a value — the schema-level column types (`SPEC.md` §3).
///
/// `null` is not a kind: any column may hold `Null` unless constrained, so
/// nullability is a constraint, not a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeKind {
    /// `bool`.
    Bool,
    /// `i64` — signed 64-bit integer (money in minor units).
    I64,
    /// `f64` — IEEE-754 double.
    F64,
    /// `text` — UTF-8.
    Text,
    /// `blob` — opaque bytes.
    Blob,
    /// `uuid` — 16 bytes, canonical string on I/O.
    Uuid,
    /// `json` — a document stored as MessagePack bytes, opaque in v1.
    Json,
    /// `timestamp` — epoch microseconds, UTC.
    Timestamp,
}

impl TypeKind {
    /// A stable lowercase name (matches `SPEC.md` §3 spelling).
    pub const fn as_str(self) -> &'static str {
        match self {
            TypeKind::Bool => "bool",
            TypeKind::I64 => "i64",
            TypeKind::F64 => "f64",
            TypeKind::Text => "text",
            TypeKind::Blob => "blob",
            TypeKind::Uuid => "uuid",
            TypeKind::Json => "json",
            TypeKind::Timestamp => "timestamp",
        }
    }
}

impl std::fmt::Display for TypeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One engine value, covering every v1 type (`SPEC.md` §3).
///
/// `Value` carries a **logical total order** (see [`Value::logical_cmp`]) that
/// the key encoding mirrors bytewise. `Eq`/`Ord` are implemented in terms of
/// that order, so for floats: every NaN is equal to every other NaN and sorts
/// above `+inf`, and `-0.0 < +0.0` (IEEE-754 total order, not `==` semantics).
#[derive(Debug, Clone)]
pub enum Value {
    /// The absence of a value; distinct from every other value.
    Null,
    /// `bool`.
    Bool(bool),
    /// `i64`.
    I64(i64),
    /// `f64`.
    F64(f64),
    /// `text` (UTF-8).
    Text(String),
    /// `blob` (opaque bytes).
    Blob(Vec<u8>),
    /// `uuid` (16 raw bytes, big-endian field order per RFC 9562).
    Uuid([u8; 16]),
    /// `json` — the document's MessagePack bytes, validated well-formed.
    Json(Vec<u8>),
    /// `timestamp` — epoch microseconds, UTC.
    Timestamp(i64),
}

impl Value {
    /// The value's kind, or `None` for `Null` (nullability is a constraint,
    /// not a type).
    pub fn kind(&self) -> Option<TypeKind> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(TypeKind::Bool),
            Value::I64(_) => Some(TypeKind::I64),
            Value::F64(_) => Some(TypeKind::F64),
            Value::Text(_) => Some(TypeKind::Text),
            Value::Blob(_) => Some(TypeKind::Blob),
            Value::Uuid(_) => Some(TypeKind::Uuid),
            Value::Json(_) => Some(TypeKind::Json),
            Value::Timestamp(_) => Some(TypeKind::Timestamp),
        }
    }

    /// Rank of the value's variant in the cross-type order. Within one typed
    /// column every value shares a rank (or is `Null`), so cross-type order
    /// only needs to be *consistent*, never semantically meaningful. `Null`
    /// ranks lowest: **nulls sort first**, engine-wide.
    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::I64(_) => 2,
            Value::F64(_) => 3,
            Value::Text(_) => 4,
            Value::Blob(_) => 5,
            Value::Uuid(_) => 6,
            Value::Timestamp(_) => 7,
            Value::Json(_) => 8,
        }
    }

    /// The engine's logical total order over values.
    ///
    /// This is the order the key encoding reproduces bytewise:
    /// `bytewise_cmp(encode(a), encode(b)) == a.logical_cmp(&b)`.
    ///
    /// - Different variants order by a fixed rank, `Null` first.
    /// - `f64` uses the IEEE-754 **total order** with every NaN canonicalized
    ///   to one positive NaN above `+inf` (so `-0.0 < +0.0`, `NaN == NaN`).
    /// - `text` compares as UTF-8 bytes (== code-point order); `blob`, `uuid`,
    ///   and `json` compare bytewise.
    pub fn logical_cmp(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::I64(a), Value::I64(b)) => a.cmp(b),
            (Value::F64(a), Value::F64(b)) => f64_total_key(*a).cmp(&f64_total_key(*b)),
            (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Value::Blob(a), Value::Blob(b)) => a.cmp(b),
            (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
            (Value::Json(a), Value::Json(b)) => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.logical_cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        self.logical_cmp(other)
    }
}

/// Map an `f64` to a `u64` whose unsigned order is the IEEE-754 total order,
/// with every NaN first canonicalized to the one positive quiet NaN (so all
/// NaNs collapse to a single greatest value).
///
/// Non-negative floats (sign bit 0) get the sign bit set, shifting them above
/// every negative; negatives (sign bit 1) are bitwise-inverted, reversing
/// their magnitude order. The mapping is bijective on canonical inputs, which
/// is what lets the key encoding decode back to the float.
pub(crate) fn f64_total_key(f: f64) -> u64 {
    let bits = if f.is_nan() {
        f64::NAN.to_bits()
    } else {
        f.to_bits()
    };
    if bits >> 63 == 0 {
        bits | (1 << 63)
    } else {
        !bits
    }
}

/// Invert [`f64_total_key`].
pub(crate) fn f64_from_total_key(key: u64) -> f64 {
    let bits = if key >> 63 == 1 {
        key & !(1 << 63)
    } else {
        !key
    };
    f64::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_total_order_edges() {
        let order = [
            f64::NEG_INFINITY,
            -1.5,
            -f64::MIN_POSITIVE,
            -0.0,
            0.0,
            f64::MIN_POSITIVE,
            1.5,
            f64::INFINITY,
            f64::NAN,
        ];
        for pair in order.windows(2) {
            assert!(
                f64_total_key(pair[0]) < f64_total_key(pair[1]),
                "{} should sort below {}",
                pair[0],
                pair[1]
            );
        }
        // All NaNs collapse to one value.
        assert_eq!(
            f64_total_key(f64::NAN),
            f64_total_key(f64::from_bits(0xFFF8_0000_0000_0001))
        );
    }

    #[test]
    fn float_key_round_trips() {
        for f in [0.0, -0.0, 1.5, -1.5, f64::INFINITY, f64::NEG_INFINITY] {
            let back = f64_from_total_key(f64_total_key(f));
            assert_eq!(back.to_bits(), f.to_bits());
        }
        assert!(f64_from_total_key(f64_total_key(f64::NAN)).is_nan());
    }

    #[test]
    fn value_eq_follows_total_order() {
        assert_eq!(Value::F64(f64::NAN), Value::F64(f64::NAN));
        assert_ne!(Value::F64(-0.0), Value::F64(0.0));
        assert!(Value::Null < Value::Bool(false));
        assert!(Value::F64(-0.0) < Value::F64(0.0));
    }
}
