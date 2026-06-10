//! The Phase 5 exit property: for any two keyable values (and composites),
//! `bytewise_cmp(encode(a), encode(b)) == logical_cmp(a, b)`, and the
//! encoding round-trips exactly.
#![allow(clippy::unwrap_used)]

use common::{Rng, SeededRng};
use types::{decode_key, encode_key, Value};

/// Curated edge cases for every keyable type.
fn curated() -> Vec<Value> {
    let mut values = vec![
        Value::Null,
        Value::Bool(false),
        Value::Bool(true),
        Value::I64(i64::MIN),
        Value::I64(-1),
        Value::I64(0),
        Value::I64(1),
        Value::I64(i64::MAX),
        Value::F64(f64::NEG_INFINITY),
        Value::F64(-1.5),
        Value::F64(-f64::MIN_POSITIVE),
        Value::F64(-0.0),
        Value::F64(0.0),
        Value::F64(f64::MIN_POSITIVE),
        Value::F64(1.5),
        Value::F64(f64::INFINITY),
        Value::F64(f64::NAN),
        Value::Text(String::new()),
        Value::Text("a".into()),
        Value::Text("a\u{0}".into()),
        Value::Text("a\u{0}b".into()),
        Value::Text("ab".into()),
        Value::Text("b".into()),
        Value::Text("éclair".into()),
        Value::Text("\u{10FFFF}".into()),
        Value::Blob(Vec::new()),
        Value::Blob(vec![0x00]),
        Value::Blob(vec![0x00, 0x00]),
        Value::Blob(vec![0x00, 0xFF]),
        Value::Blob(vec![0x01]),
        Value::Blob(vec![0xFF; 40]),
        Value::Uuid([0x00; 16]),
        Value::Uuid([0xFF; 16]),
        Value::Timestamp(i64::MIN),
        Value::Timestamp(-1),
        Value::Timestamp(0),
        Value::Timestamp(1_700_000_000_000_000),
        Value::Timestamp(i64::MAX),
    ];
    // A near-identical uuid pair differing in the last byte.
    let mut u = [0xAB; 16];
    values.push(Value::Uuid(u));
    u[15] = 0xAC;
    values.push(Value::Uuid(u));
    values
}

fn random_value(rng: &SeededRng) -> Value {
    match rng.next_u64() % 8 {
        0 => Value::Null,
        1 => Value::Bool(rng.next_u64() % 2 == 1),
        2 => Value::I64(rng.next_u64() as i64),
        3 => {
            // Random bit patterns hit subnormals, infs, and NaNs too.
            Value::F64(f64::from_bits(rng.next_u64()))
        }
        4 => {
            let len = (rng.next_u64() % 12) as usize;
            let s: String = (0..len)
                .map(|_| char::from_u32((rng.next_u64() % 0x80) as u32).unwrap_or('x'))
                .collect();
            Value::Text(s)
        }
        5 => {
            let len = (rng.next_u64() % 12) as usize;
            // Bias toward 0x00/0xFF to stress the escape coding.
            let b: Vec<u8> = (0..len)
                .map(|_| match rng.next_u64() % 4 {
                    0 => 0x00,
                    1 => 0xFF,
                    _ => (rng.next_u64() % 256) as u8,
                })
                .collect();
            Value::Blob(b)
        }
        6 => {
            let mut u = [0u8; 16];
            for chunk in u.chunks_mut(8) {
                chunk.copy_from_slice(&rng.next_u64().to_be_bytes()[..chunk.len()]);
            }
            Value::Uuid(u)
        }
        _ => Value::Timestamp(rng.next_u64() as i64),
    }
}

/// Assert the property for one pair of composite keys.
fn check_pair(a: &[Value], b: &[Value]) {
    let ea = encode_key(a).unwrap();
    let eb = encode_key(b).unwrap();
    let logical = a.cmp(b); // Value's Ord IS logical_cmp; slices compare lexicographically.
    assert_eq!(
        ea.cmp(&eb),
        logical,
        "bytewise order disagrees with logical order for {a:?} vs {b:?}"
    );
}

#[test]
fn bytewise_order_equals_logical_order_for_curated_values() {
    let values = curated();
    for a in &values {
        for b in &values {
            check_pair(std::slice::from_ref(a), std::slice::from_ref(b));
        }
    }
}

#[test]
fn bytewise_order_equals_logical_order_for_random_values() {
    for seed in 1..=6u64 {
        let rng = SeededRng::new(seed);
        let values: Vec<Value> = (0..120).map(|_| random_value(&rng)).collect();
        for a in &values {
            for b in &values {
                check_pair(std::slice::from_ref(a), std::slice::from_ref(b));
            }
        }
    }
}

#[test]
fn composite_keys_order_lexicographically() {
    // Curated composites differing at each position, mixing variable-length
    // components with following components (the escape coding's hard case).
    let tuples: Vec<Vec<Value>> = vec![
        vec![Value::Text("a".into()), Value::I64(99)],
        vec![Value::Text("a".into()), Value::I64(100)],
        vec![Value::Text("a\u{0}".into()), Value::I64(0)],
        vec![Value::Text("ab".into()), Value::I64(-5)],
        vec![Value::Null, Value::Text("z".into())],
        vec![Value::Bool(true), Value::Null],
        vec![Value::Blob(vec![0x00]), Value::Blob(vec![0xFF])],
        vec![Value::Blob(vec![0x00, 0x00]), Value::Blob(Vec::new())],
        vec![Value::I64(7)],
        vec![Value::I64(7), Value::Null],
        vec![Value::I64(7), Value::Bool(false)],
    ];
    for a in &tuples {
        for b in &tuples {
            check_pair(a, b);
        }
    }

    // Random composites of length 1..=3.
    for seed in 7..=10u64 {
        let rng = SeededRng::new(seed);
        let tuples: Vec<Vec<Value>> = (0..60)
            .map(|_| {
                let len = 1 + (rng.next_u64() % 3) as usize;
                (0..len).map(|_| random_value(&rng)).collect()
            })
            .collect();
        for a in &tuples {
            for b in &tuples {
                check_pair(a, b);
            }
        }
    }
}

#[test]
fn keys_round_trip_exactly() {
    let rng = SeededRng::new(42);
    let mut all: Vec<Vec<Value>> = curated().into_iter().map(|v| vec![v]).collect();
    for _ in 0..200 {
        let len = 1 + (rng.next_u64() % 3) as usize;
        all.push((0..len).map(|_| random_value(&rng)).collect());
    }
    for key in &all {
        let encoded = encode_key(key).unwrap();
        let decoded = decode_key(&encoded).unwrap();
        assert_eq!(&decoded, key, "round-trip mismatch");
        // Bit-exactness for floats (Value's Eq treats all NaNs equal).
        for (d, k) in decoded.iter().zip(key) {
            if let (Value::F64(d), Value::F64(k)) = (d, k) {
                let canonical = if k.is_nan() { f64::NAN } else { *k };
                assert_eq!(d.to_bits(), canonical.to_bits());
            }
        }
    }
}

#[test]
fn corrupt_key_bytes_never_panic() {
    let rng = SeededRng::new(99);
    let base = encode_key(&[
        Value::Text("hello\u{0}world".into()),
        Value::I64(-42),
        Value::Uuid([7; 16]),
    ])
    .unwrap();

    // Truncations at every length.
    for n in 0..base.len() {
        let _ = decode_key(&base[..n]);
    }
    // Single-byte mutations.
    for i in 0..base.len() {
        for _ in 0..4 {
            let mut bytes = base.clone();
            bytes[i] = (rng.next_u64() % 256) as u8;
            let _ = decode_key(&bytes);
        }
    }
    // Random garbage.
    for _ in 0..500 {
        let len = (rng.next_u64() % 40) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (rng.next_u64() % 256) as u8).collect();
        let _ = decode_key(&bytes);
    }
}
