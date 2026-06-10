//! Generated and engine-managed values under the writer: auto-increment,
//! rowversion, `now` / `on_update: now`, and `uuid_v7` — including
//! persistence of the auto-increment sequence across reopen.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use catalog::{Catalog, CatalogError, ColumnDef, TableDef};
use common::{ManualClock, MemoryBackend, SeededRng};
use types::{TypeKind, Value};

type Backend = Arc<MemoryBackend>;

const T0: i64 = 1_700_000_000_000_000;

fn new_catalog() -> (Catalog<Backend>, Backend, Arc<ManualClock>) {
    let backend: Backend = Arc::new(MemoryBackend::new());
    let clock = Arc::new(ManualClock::new(T0));
    let cat = Catalog::create(
        Arc::clone(&backend),
        clock.clone(),
        Arc::new(SeededRng::new(99)),
    )
    .unwrap();
    (cat, backend, clock)
}

fn events() -> TableDef {
    TableDef::new(
        "events",
        vec![
            ColumnDef::new("id", TypeKind::I64).auto_increment(),
            ColumnDef::new("uid", TypeKind::Uuid).default_uuid_v7(),
            ColumnDef::new("label", TypeKind::Text),
            ColumnDef::new("version", TypeKind::I64).rowversion(),
            ColumnDef::new("created_at", TypeKind::Timestamp).default_now(),
            ColumnDef::new("updated_at", TypeKind::Timestamp).on_update_now(),
        ],
        vec!["id"],
    )
}

fn label(s: &str) -> Vec<(String, Value)> {
    vec![("label".to_string(), Value::Text(s.into()))]
}

#[test]
fn auto_increment_is_sequential_and_survives_reopen() {
    let (cat, backend, _clock) = new_catalog();
    cat.create_table(events()).unwrap();

    for expect in 1..=5i64 {
        let row = cat.insert("events", label("x")).unwrap();
        assert_eq!(row[0], Value::I64(expect));
    }
    // An explicit key advances the sequence past itself.
    cat.insert(
        "events",
        vec![
            ("id".to_string(), Value::I64(100)),
            ("label".to_string(), Value::Text("explicit".into())),
        ],
    )
    .unwrap();
    assert_eq!(
        cat.insert("events", label("x")).unwrap()[0],
        Value::I64(101)
    );

    // A rejected transaction may burn ids but never reuses one.
    let _ = cat.insert(
        "events",
        vec![
            ("label".to_string(), Value::Text("bad".into())),
            ("ghost".to_string(), Value::I64(0)),
        ],
    );

    // The sequence is durable: reopen and keep counting, no reuse.
    drop(cat);
    let cat = Catalog::open(
        backend,
        Arc::new(ManualClock::new(T0)),
        Arc::new(SeededRng::new(1)),
    )
    .unwrap();
    let row = cat.insert("events", label("after-reopen")).unwrap();
    assert_eq!(row[0], Value::I64(102));
}

#[test]
fn rowversion_starts_at_one_and_bumps_every_update() {
    let (cat, _b, _clock) = new_catalog();
    cat.create_table(events()).unwrap();
    let row = cat.insert("events", label("v")).unwrap();
    assert_eq!(row[3], Value::I64(1));

    let pk = vec![row[0].clone()];
    for expect in 2..=4i64 {
        let row = cat
            .update("events", pk.clone(), label(&format!("v{expect}")))
            .unwrap();
        assert_eq!(row[3], Value::I64(expect), "rowversion must bump");
    }

    // Explicit writes to the engine-managed column are rejected.
    assert!(matches!(
        cat.update("events", pk, vec![("version".to_string(), Value::I64(99))]),
        Err(CatalogError::EngineManagedColumn { .. })
    ));
}

#[test]
fn now_and_on_update_now_track_the_injected_clock() {
    let (cat, _b, clock) = new_catalog();
    cat.create_table(events()).unwrap();

    let row = cat.insert("events", label("t")).unwrap();
    // Both stamps equal the insert-time clock.
    assert_eq!(row[4], Value::Timestamp(T0));
    assert_eq!(row[5], Value::Timestamp(T0));

    clock.advance(5_000_000);
    let pk = vec![row[0].clone()];
    let row = cat.update("events", pk.clone(), label("t2")).unwrap();
    // created_at frozen, updated_at refreshed.
    assert_eq!(row[4], Value::Timestamp(T0));
    assert_eq!(row[5], Value::Timestamp(T0 + 5_000_000));

    clock.advance(1_000_000);
    let row = cat.update("events", pk, label("t3")).unwrap();
    assert_eq!(row[5], Value::Timestamp(T0 + 6_000_000));

    // on_update columns cannot be written explicitly (insert or update).
    assert!(matches!(
        cat.insert(
            "events",
            vec![("updated_at".to_string(), Value::Timestamp(0))]
        ),
        Err(CatalogError::EngineManagedColumn { .. })
    ));
}

#[test]
fn uuid_v7_defaults_are_unique_and_time_ordered() {
    let (cat, _b, clock) = new_catalog();
    cat.create_table(events()).unwrap();

    let mut last: Option<[u8; 16]> = None;
    for i in 0..50 {
        if i % 10 == 0 {
            clock.advance(1_000);
        }
        let row = cat.insert("events", label("u")).unwrap();
        let Value::Uuid(u) = row[1] else {
            panic!("expected a uuid, got {:?}", row[1]);
        };
        assert_eq!(u[6] >> 4, 0x7, "must be UUIDv7");
        if let Some(prev) = last {
            assert!(u > prev, "uuid_v7 defaults must be monotonic in-run");
        }
        last = Some(u);
    }
}
