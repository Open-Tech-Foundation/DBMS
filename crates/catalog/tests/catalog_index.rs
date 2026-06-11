//! Phase 7 exit criteria: index DDL, automatic maintenance kept atomic with
//! base-row writes, unique-violation detection (insert, update, and
//! `create index` backfill), and `validate()` cross-checking base ↔ index.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use catalog::{implicit_index_name, Catalog, CatalogError, ColumnDef, IndexDef, TableDef};
use common::{ManualClock, MemoryBackend, SeededRng};
use types::{TypeKind, Value};

type Backend = Arc<MemoryBackend>;

fn new_catalog() -> (Catalog<Backend>, Backend) {
    let backend: Backend = Arc::new(MemoryBackend::new());
    let cat = Catalog::create(
        Arc::clone(&backend),
        Arc::new(ManualClock::new(1_700_000_000_000_000)),
        Arc::new(SeededRng::new(11)),
    )
    .unwrap();
    (cat, backend)
}

fn people() -> TableDef {
    TableDef::new(
        "people",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("email", TypeKind::Text).unique(),
            ColumnDef::new("city", TypeKind::Text),
            ColumnDef::new("age", TypeKind::I64),
        ],
        vec!["id"],
    )
    .index(IndexDef::new("by_city_age", vec!["city", "age"]))
}

fn person(id: i64, email: Option<&str>, city: &str, age: i64) -> Vec<(String, Value)> {
    let mut row = vec![
        ("id".to_string(), Value::I64(id)),
        ("city".to_string(), Value::Text(city.into())),
        ("age".to_string(), Value::I64(age)),
    ];
    row.push((
        "email".to_string(),
        email.map_or(Value::Null, |e| Value::Text(e.into())),
    ));
    row
}

#[test]
fn index_definitions_persist_and_unique_columns_get_implicit_indexes() {
    let (cat, backend) = new_catalog();
    cat.create_table(people()).unwrap();
    drop(cat);

    let cat = Catalog::open(
        backend,
        Arc::new(ManualClock::new(0)),
        Arc::new(SeededRng::new(1)),
    )
    .unwrap();
    let def = cat.snapshot().table("people").unwrap();
    let names: Vec<&str> = def.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "by_city_age",
            implicit_index_name("people", "email").as_str()
        ]
    );
    assert!(!def.indexes[0].unique);
    assert!(def.indexes[1].unique);
    cat.snapshot().validate().unwrap();
}

#[test]
fn maintenance_keeps_indexes_in_sync_through_dml() {
    let (cat, _b) = new_catalog();
    cat.create_table(people()).unwrap();

    cat.insert("people", person(1, Some("a@x"), "oslo", 30))
        .unwrap();
    cat.insert("people", person(2, Some("b@x"), "rome", 40))
        .unwrap();
    cat.insert("people", person(3, None, "oslo", 50)).unwrap();
    cat.snapshot().validate().unwrap();

    // Update moving a row across index keys.
    cat.update(
        "people",
        vec![Value::I64(1)],
        vec![
            ("city".to_string(), Value::Text("rome".into())),
            ("email".to_string(), Value::Text("a2@x".into())),
        ],
    )
    .unwrap();
    cat.snapshot().validate().unwrap();

    // Update that leaves index keys unchanged.
    cat.update(
        "people",
        vec![Value::I64(2)],
        vec![("age".to_string(), Value::I64(40))],
    )
    .unwrap();
    cat.snapshot().validate().unwrap();

    // Delete removes entries everywhere.
    assert!(cat.delete("people", vec![Value::I64(1)]).unwrap());
    cat.snapshot().validate().unwrap();
}

#[test]
fn unique_violations_via_indexes() {
    let (cat, _b) = new_catalog();
    cat.create_table(people()).unwrap();
    cat.insert("people", person(1, Some("a@x"), "oslo", 30))
        .unwrap();

    // Insert collision on the implicit unique index.
    assert!(matches!(
        cat.insert("people", person(2, Some("a@x"), "rome", 40)),
        Err(CatalogError::UniqueViolation { .. })
    ));
    // Update collision.
    cat.insert("people", person(2, Some("b@x"), "rome", 40))
        .unwrap();
    assert!(matches!(
        cat.update(
            "people",
            vec![Value::I64(2)],
            vec![("email".to_string(), Value::Text("a@x".into()))],
        ),
        Err(CatalogError::UniqueViolation { .. })
    ));
    // NULLs never conflict; freeing a value makes it claimable again.
    cat.update(
        "people",
        vec![Value::I64(1)],
        vec![("email".to_string(), Value::Null)],
    )
    .unwrap();
    cat.update(
        "people",
        vec![Value::I64(2)],
        vec![("email".to_string(), Value::Text("a@x".into()))],
    )
    .unwrap();
    cat.snapshot().validate().unwrap();

    // A multi-row insert with one collision leaves base AND indexes untouched.
    let before = cat.snapshot();
    let result = cat.insert_many(
        "people",
        vec![
            person(10, Some("ten@x"), "oslo", 1),
            person(11, Some("a@x"), "oslo", 2), // taken by row 2
        ],
    );
    assert!(matches!(result, Err(CatalogError::UniqueViolation { .. })));
    let after = cat.snapshot();
    assert_eq!(
        after.scan("people").unwrap(),
        before.scan("people").unwrap()
    );
    after.validate().unwrap();
}

#[test]
fn create_index_backfills_and_rejects_duplicate_data() {
    let (cat, _b) = new_catalog();
    cat.create_table(people()).unwrap();
    for i in 0..30 {
        cat.insert("people", person(i, None, &format!("c{}", i % 5), i))
            .unwrap();
    }

    // Backfilled non-unique index over existing rows.
    cat.create_index("people", IndexDef::new("by_age", vec!["age"]))
        .unwrap();
    cat.snapshot().validate().unwrap();

    // A unique index over duplicate data is rejected whole.
    assert!(matches!(
        cat.create_index("people", IndexDef::new("uniq_city", vec!["city"]).unique()),
        Err(CatalogError::UniqueViolation { .. })
    ));
    assert!(cat
        .snapshot()
        .table("people")
        .unwrap()
        .index_pos("uniq_city")
        .is_none());
    cat.snapshot().validate().unwrap();

    // A unique index over distinct data backfills fine; duplicate names don't.
    cat.create_index("people", IndexDef::new("uniq_age", vec!["age"]).unique())
        .unwrap();
    assert!(matches!(
        cat.create_index("people", IndexDef::new("by_age", vec!["city"])),
        Err(CatalogError::IndexExists { .. })
    ));
    // And the new unique index enforces from now on.
    assert!(matches!(
        cat.insert("people", person(100, None, "c0", 7)),
        Err(CatalogError::UniqueViolation { .. })
    ));
    cat.snapshot().validate().unwrap();
}

#[test]
fn drop_index_frees_pages_and_protects_unique_backings() {
    let (cat, _b) = new_catalog();
    cat.create_table(people()).unwrap();
    for i in 0..100 {
        cat.insert(
            "people",
            person(i, Some(&format!("u{i}@x")), &format!("city-{i}"), i),
        )
        .unwrap();
    }

    // The implicit backing of a unique column cannot be dropped…
    assert!(matches!(
        cat.drop_index("people", &implicit_index_name("people", "email")),
        Err(CatalogError::InvalidSchema { .. })
    ));
    // …an unknown index is NotFound…
    assert!(matches!(
        cat.drop_index("people", "ghost"),
        Err(CatalogError::UnknownIndex { .. })
    ));
    // …and a user index drops, freeing its pages.
    let before = cat.validate().unwrap();
    cat.drop_index("people", "by_city_age").unwrap();
    cat.insert("people", person(200, None, "x", 0)).unwrap(); // trigger reclaim
    let after = cat.validate().unwrap();
    assert!(
        after.free_ids > before.free_ids,
        "dropping an index over 100 rows should free pages"
    );
    cat.snapshot().validate().unwrap();
}

#[test]
fn drop_table_frees_index_trees_too() {
    let (cat, _b) = new_catalog();
    cat.create_table(people()).unwrap();
    for i in 0..100 {
        cat.insert(
            "people",
            person(i, Some(&format!("u{i}@x")), &format!("city-{i}"), i),
        )
        .unwrap();
    }
    let before = cat.validate().unwrap();
    cat.drop_table("people").unwrap();
    cat.create_table(TableDef::new(
        "t",
        vec![ColumnDef::new("id", TypeKind::I64)],
        vec!["id"],
    ))
    .unwrap(); // trigger reclaim
    let after = cat.validate().unwrap();
    assert!(
        after.free_ids > before.free_ids + 3,
        "base and index trees should all be freed (before {}, after {})",
        before.free_ids,
        after.free_ids
    );
}

#[test]
fn a_pinned_snapshot_validates_against_its_own_version() {
    let (cat, _b) = new_catalog();
    cat.create_table(people()).unwrap();
    for i in 0..20 {
        cat.insert("people", person(i, Some(&format!("u{i}@x")), "oslo", i))
            .unwrap();
    }
    let pinned = cat.snapshot();

    // Churn after the pin: updates, deletes, a new index.
    for i in 0..20 {
        if i % 3 == 0 {
            cat.delete("people", vec![Value::I64(i)]).unwrap();
        } else {
            cat.update(
                "people",
                vec![Value::I64(i)],
                vec![("city".to_string(), Value::Text(format!("c{i}")))],
            )
            .unwrap();
        }
    }
    cat.create_index("people", IndexDef::new("by_age", vec!["age"]))
        .unwrap();

    // The pinned snapshot is still internally consistent at its version —
    // and doesn't see the new index.
    pinned.validate().unwrap();
    assert!(pinned
        .table("people")
        .unwrap()
        .index_pos("by_age")
        .is_none());
    assert_eq!(pinned.scan("people").unwrap().len(), 20);
    cat.snapshot().validate().unwrap();
}
