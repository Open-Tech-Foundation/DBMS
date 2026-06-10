//! UUIDv7 exit criteria: correct version/variant bits, time-ordering, and
//! strict monotonicity within a run — even when the clock stalls or steps
//! backward.
#![allow(clippy::unwrap_used)]

use common::{ManualClock, SeededRng};
use types::{uuid_from_str, uuid_to_string, UuidV7Gen};

#[test]
fn version_and_variant_bits_are_correct() {
    let clock = std::sync::Arc::new(ManualClock::new(1_700_000_000_000_000));
    let rng = std::sync::Arc::new(SeededRng::new(1));
    let gen = UuidV7Gen::new(clock.clone(), rng.clone());
    for _ in 0..100 {
        let u = gen.next_uuid();
        assert_eq!(u[6] >> 4, 0x7, "version nibble must be 7");
        assert_eq!(u[8] >> 6, 0b10, "variant must be 0b10");
    }
}

#[test]
fn timestamp_field_tracks_the_clock() {
    let micros = 1_700_000_000_123_456i64;
    let clock = std::sync::Arc::new(ManualClock::new(micros));
    let rng = std::sync::Arc::new(SeededRng::new(2));
    let gen = UuidV7Gen::new(clock.clone(), rng.clone());
    let u = gen.next_uuid();

    let mut ms_bytes = [0u8; 8];
    ms_bytes[2..].copy_from_slice(&u[..6]);
    let ms = u64::from_be_bytes(ms_bytes);
    assert_eq!(ms, (micros / 1000) as u64);
}

#[test]
fn strictly_monotonic_within_a_run() {
    let clock = std::sync::Arc::new(ManualClock::new(1_700_000_000_000_000));
    let rng = std::sync::Arc::new(SeededRng::new(3));
    let gen = UuidV7Gen::new(clock.clone(), rng.clone());

    let mut last = gen.next_uuid();
    for i in 0..20_000 {
        // Mix of: same millisecond (most often), advancing, and stepping back.
        match i % 100 {
            0..=4 => clock.advance(1_000),
            5 => clock.advance(-3_000),
            6 => clock.advance(250),
            _ => {}
        }
        let next = gen.next_uuid();
        assert!(
            next > last,
            "uuid did not increase at step {i}: {} then {}",
            uuid_to_string(&last),
            uuid_to_string(&next)
        );
        last = next;
    }
}

#[test]
fn deterministic_under_the_same_seed_and_clock() {
    let make = |seed| {
        let clock = std::sync::Arc::new(ManualClock::new(42_000));
        let rng = std::sync::Arc::new(SeededRng::new(seed));
        let gen = UuidV7Gen::new(clock.clone(), rng.clone());
        (0..50).map(|_| gen.next_uuid()).collect::<Vec<_>>()
    };
    assert_eq!(make(7), make(7), "same seed must reproduce the run");
    assert_ne!(make(7), make(8), "different seeds must diverge");
}

#[test]
fn canonical_string_round_trips() {
    let clock = std::sync::Arc::new(ManualClock::new(1_700_000_000_000_000));
    let rng = std::sync::Arc::new(SeededRng::new(4));
    let gen = UuidV7Gen::new(clock.clone(), rng.clone());
    for _ in 0..50 {
        let u = gen.next_uuid();
        let s = uuid_to_string(&u);
        assert_eq!(s.len(), 36);
        assert_eq!(uuid_from_str(&s).unwrap(), u);
    }
}
