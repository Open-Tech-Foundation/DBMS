//! Row-encoding and MessagePack wire-mapping round-trips, json
//! well-formedness, and decoder robustness on hostile bytes.
#![allow(clippy::unwrap_used)]

use common::{Rng, SeededRng};
use types::{
    decode_row, decode_value, encode_row, encode_value, validate_json, MsgpackError, TypeError,
    TypeKind, Value,
};

/// A small well-formed MessagePack document: `{"a": [1, null, true], "b": "x"}`.
fn sample_doc() -> Vec<u8> {
    vec![
        0x82, // map, 2 entries
        0xA1, b'a', // "a"
        0x93, 0x01, 0xC0, 0xC3, // [1, nil, true]
        0xA1, b'b', // "b"
        0xA1, b'x', // "x"
    ]
}

fn sample_row() -> Vec<Value> {
    vec![
        Value::Null,
        Value::Bool(true),
        Value::I64(i64::MIN),
        Value::F64(-0.0),
        Value::Text("héllo\u{0}wörld".into()),
        Value::Blob(vec![0x00, 0xFF, 0xC1]),
        Value::Uuid([0xAB; 16]),
        Value::Json(sample_doc()),
        Value::Timestamp(1_700_000_000_000_000),
    ]
}

#[test]
fn rows_round_trip_exactly() {
    let row = sample_row();
    assert_eq!(decode_row(&encode_row(&row).unwrap()).unwrap(), row);
    assert_eq!(decode_row(&encode_row(&[]).unwrap()).unwrap(), vec![]);

    let rng = SeededRng::new(11);
    for _ in 0..200 {
        let len = (rng.next_u64() % 10) as usize;
        let row: Vec<Value> = (0..len).map(|_| random_value(&rng)).collect();
        assert_eq!(decode_row(&encode_row(&row).unwrap()).unwrap(), row);
    }
}

fn random_value(rng: &SeededRng) -> Value {
    match rng.next_u64() % 9 {
        0 => Value::Null,
        1 => Value::Bool(rng.next_u64() % 2 == 1),
        2 => Value::I64(rng.next_u64() as i64),
        3 => Value::F64(f64::from_bits(rng.next_u64())),
        4 => {
            let len = (rng.next_u64() % 20) as usize;
            Value::Text(
                (0..len)
                    .map(|_| char::from_u32((rng.next_u64() % 0x80) as u32).unwrap_or('x'))
                    .collect(),
            )
        }
        5 => {
            let len = (rng.next_u64() % 20) as usize;
            Value::Blob((0..len).map(|_| (rng.next_u64() % 256) as u8).collect())
        }
        6 => {
            let mut u = [0u8; 16];
            for chunk in u.chunks_mut(8) {
                chunk.copy_from_slice(&rng.next_u64().to_be_bytes()[..chunk.len()]);
            }
            Value::Uuid(u)
        }
        7 => Value::Json(sample_doc()),
        _ => Value::Timestamp(rng.next_u64() as i64),
    }
}

#[test]
fn corrupt_row_bytes_never_panic_and_are_typed() {
    let base = encode_row(&sample_row()).unwrap();
    let rng = SeededRng::new(23);

    for n in 0..base.len() {
        if let Err(e) = decode_row(&base[..n]) {
            assert!(matches!(e, TypeError::RowCorrupt(_)), "untyped: {e}");
        }
    }
    for i in 0..base.len() {
        for _ in 0..4 {
            let mut bytes = base.clone();
            bytes[i] = (rng.next_u64() % 256) as u8;
            let _ = decode_row(&bytes);
        }
    }
    for _ in 0..500 {
        let len = (rng.next_u64() % 60) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() % 256) as u8).collect();
        let _ = decode_row(&bytes);
    }
}

#[test]
fn wire_values_round_trip_for_every_kind() {
    let cases: Vec<(Value, TypeKind)> = vec![
        (Value::Bool(false), TypeKind::Bool),
        (Value::Bool(true), TypeKind::Bool),
        (Value::I64(0), TypeKind::I64),
        (Value::I64(127), TypeKind::I64),
        (Value::I64(-32), TypeKind::I64),
        (Value::I64(255), TypeKind::I64),
        (Value::I64(-129), TypeKind::I64),
        (Value::I64(65_535), TypeKind::I64),
        (Value::I64(-40_000), TypeKind::I64),
        (Value::I64(4_294_967_295), TypeKind::I64),
        (Value::I64(i64::MIN), TypeKind::I64),
        (Value::I64(i64::MAX), TypeKind::I64),
        (Value::F64(-0.0), TypeKind::F64),
        (Value::F64(f64::INFINITY), TypeKind::F64),
        (Value::Text(String::new()), TypeKind::Text),
        (Value::Text("x".repeat(40)), TypeKind::Text),
        (Value::Text("y".repeat(300)), TypeKind::Text),
        (Value::Text("z".repeat(70_000)), TypeKind::Text),
        (Value::Blob(vec![1, 2, 3]), TypeKind::Blob),
        (Value::Blob(vec![7; 300]), TypeKind::Blob),
        (Value::Blob(vec![8; 70_000]), TypeKind::Blob),
        (Value::Uuid([0x5A; 16]), TypeKind::Uuid),
        (Value::Json(sample_doc()), TypeKind::Json),
        (
            Value::Timestamp(-62_135_596_800_000_000),
            TypeKind::Timestamp,
        ),
        (Value::Timestamp(i64::MAX), TypeKind::Timestamp),
    ];
    for (value, kind) in &cases {
        let wire = encode_value(value).unwrap();
        let back = decode_value(&wire, *kind).unwrap();
        assert_eq!(&back, value, "round-trip mismatch for {kind}");
        // nil decodes as Null under any kind.
        assert_eq!(decode_value(&[0xC0], *kind).unwrap(), Value::Null);
    }

    // NaN round-trips as NaN.
    let wire = encode_value(&Value::F64(f64::NAN)).unwrap();
    assert!(matches!(decode_value(&wire, TypeKind::F64).unwrap(), Value::F64(f) if f.is_nan()));
}

#[test]
fn wire_type_mismatches_are_rejected() {
    let int = encode_value(&Value::I64(5)).unwrap();
    let text = encode_value(&Value::Text("hi".into())).unwrap();

    assert!(matches!(
        decode_value(&int, TypeKind::Text),
        Err(TypeError::WrongWireType { .. })
    ));
    assert!(matches!(
        decode_value(&text, TypeKind::I64),
        Err(TypeError::WrongWireType { .. })
    ));
    assert!(matches!(
        decode_value(&text, TypeKind::Uuid),
        Err(TypeError::BadUuid)
    ));

    // A u64 beyond i64::MAX cannot become an i64.
    let mut big = vec![0xCF];
    big.extend_from_slice(&u64::MAX.to_be_bytes());
    assert!(matches!(
        decode_value(&big, TypeKind::I64),
        Err(TypeError::Msgpack(MsgpackError::IntOutOfRange { .. }))
    ));

    // Trailing bytes after a complete value are rejected.
    let mut padded = int.clone();
    padded.push(0x00);
    assert!(matches!(
        decode_value(&padded, TypeKind::I64),
        Err(TypeError::Msgpack(MsgpackError::TrailingBytes { .. }))
    ));
}

#[test]
fn well_formed_json_is_accepted() {
    validate_json(&sample_doc()).unwrap();
    validate_json(&[0xC0]).unwrap(); // nil
    validate_json(&[0x90]).unwrap(); // []
    validate_json(&[0x80]).unwrap(); // {}
    validate_json(&[0x2A]).unwrap(); // 42

    // Nesting exactly at the limit is fine: 64 nested arrays around a scalar.
    let mut deep = vec![0x91; types::MAX_JSON_DEPTH];
    deep.push(0x01);
    validate_json(&deep).unwrap();
}

#[test]
fn malformed_json_is_rejected() {
    // Truncated document (map of 2, only one entry).
    let mut truncated = sample_doc();
    truncated.truncate(truncated.len() - 3);
    assert!(matches!(
        validate_json(&truncated),
        Err(TypeError::Msgpack(MsgpackError::Truncated))
    ));

    // Trailing bytes.
    let mut trailing = sample_doc();
    trailing.push(0x00);
    assert!(matches!(
        validate_json(&trailing),
        Err(TypeError::Msgpack(MsgpackError::TrailingBytes { .. }))
    ));

    // A depth bomb: one past the limit.
    let mut bomb = vec![0x91; types::MAX_JSON_DEPTH + 1];
    bomb.push(0x01);
    assert!(matches!(
        validate_json(&bomb),
        Err(TypeError::Msgpack(MsgpackError::DepthExceeded { .. }))
    ));

    // The reserved byte.
    assert!(matches!(
        validate_json(&[0xC1]),
        Err(TypeError::Msgpack(MsgpackError::Reserved))
    ));

    // Ext types are not allowed in v1 documents (fixext1 here).
    assert!(matches!(
        validate_json(&[0xD4, 0x01, 0x00]),
        Err(TypeError::Msgpack(MsgpackError::ExtNotAllowed))
    ));

    // Empty input.
    assert!(validate_json(&[]).is_err());

    // A declared-huge array with no items (length lies past the input).
    assert!(validate_json(&[0xDD, 0xFF, 0xFF, 0xFF, 0xFF]).is_err());
}

#[test]
fn hostile_wire_bytes_never_panic() {
    let rng = SeededRng::new(37);
    let kinds = [
        TypeKind::Bool,
        TypeKind::I64,
        TypeKind::F64,
        TypeKind::Text,
        TypeKind::Blob,
        TypeKind::Uuid,
        TypeKind::Json,
        TypeKind::Timestamp,
    ];
    for _ in 0..2000 {
        let len = (rng.next_u64() % 30) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() % 256) as u8).collect();
        let _ = validate_json(&bytes);
        for kind in kinds {
            let _ = decode_value(&bytes, kind);
        }
    }
}
