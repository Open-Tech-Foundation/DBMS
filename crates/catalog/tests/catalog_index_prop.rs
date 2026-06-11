//! The Phase 7 property test: after a seeded random DML stream over a table
//! with several indexes (composite and unique included), **every index
//! matches a brute-force scan of the base table**, the model agrees with the
//! base, and `validate()` proves it all — reproducible from the seed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use catalog::{Catalog, CatalogError, ColumnDef, IndexDef, TableDef};
use common::{ManualClock, MemoryBackend, Rng, SeededRng};
use types::{TypeKind, Value};

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const STEPS: u32 = 400;
const ID_SPACE: u64 = 60;

/// In-memory model: pk → (email, city, age).
type Model = BTreeMap<i64, (Option<String>, String, i64)>;

fn table() -> TableDef {
    TableDef::new(
        "t",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("email", TypeKind::Text).unique(),
            ColumnDef::new("city", TypeKind::Text),
            ColumnDef::new("age", TypeKind::I64),
        ],
        vec!["id"],
    )
    .index(IndexDef::new("by_city_age", vec!["city", "age"]))
    .index(IndexDef::new("by_age", vec!["age"]))
}

fn rand_email(rng: &SeededRng) -> Option<String> {
    // Small space → frequent unique collisions; None → NULL (never conflicts).
    match rng.next_u64() % 4 {
        0 => None,
        _ => Some(format!("e{}@x", rng.next_u64() % 40)),
    }
}

fn rand_city(rng: &SeededRng) -> String {
    format!("c{}", rng.next_u64() % 8)
}

fn row_values(id: i64, email: &Option<String>, city: &str, age: i64) -> Vec<(String, Value)> {
    vec![
        ("id".to_string(), Value::I64(id)),
        (
            "email".to_string(),
            email
                .as_ref()
                .map_or(Value::Null, |e| Value::Text(e.clone())),
        ),
        ("city".to_string(), Value::Text(city.to_string())),
        ("age".to_string(), Value::I64(age)),
    ]
}

/// The model's view of which emails are taken (excluding `but_not`).
fn email_taken(model: &Model, email: &str, but_not: Option<i64>) -> bool {
    model
        .iter()
        .any(|(id, (e, _, _))| Some(*id) != but_not && e.as_deref() == Some(email))
}

fn run(seed: u64) {
    let rng = SeededRng::new(seed);
    let cat = Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_700_000_000_000_000)),
        Arc::new(SeededRng::new(seed ^ 0xFFFF)),
    )
    .unwrap();
    cat.create_table(table()).unwrap();
    let mut model = Model::new();

    for step in 0..STEPS {
        let id = (rng.next_u64() % ID_SPACE) as i64;
        match rng.next_u64() % 4 {
            // Insert.
            0..=1 => {
                let email = rand_email(&rng);
                let city = rand_city(&rng);
                let age = (rng.next_u64() % 50) as i64;
                let result = cat.insert("t", row_values(id, &email, &city, age));
                let conflict = model.contains_key(&id)
                    || email
                        .as_deref()
                        .is_some_and(|e| email_taken(&model, e, None));
                match result {
                    Ok(_) => {
                        assert!(!conflict, "seed {seed} step {step}: accepted a conflict");
                        model.insert(id, (email, city, age));
                    }
                    Err(
                        CatalogError::DuplicateKey { .. } | CatalogError::UniqueViolation { .. },
                    ) => {
                        assert!(conflict, "seed {seed} step {step}: spurious rejection");
                    }
                    Err(e) => panic!("seed {seed} step {step}: unexpected error {e}"),
                }
            }
            // Update (email and/or city).
            2 => {
                let email = rand_email(&rng);
                let city = rand_city(&rng);
                let sets = vec![
                    (
                        "email".to_string(),
                        email
                            .as_ref()
                            .map_or(Value::Null, |e| Value::Text(e.clone())),
                    ),
                    ("city".to_string(), Value::Text(city.clone())),
                ];
                let result = cat.update("t", vec![Value::I64(id)], sets);
                if !model.contains_key(&id) {
                    assert!(
                        matches!(result, Err(CatalogError::RowNotFound { .. })),
                        "seed {seed} step {step}: update of missing row"
                    );
                    continue;
                }
                let conflict = email
                    .as_deref()
                    .is_some_and(|e| email_taken(&model, e, Some(id)));
                match result {
                    Ok(_) => {
                        assert!(!conflict, "seed {seed} step {step}: accepted a conflict");
                        if let Some(entry) = model.get_mut(&id) {
                            entry.0 = email;
                            entry.1 = city;
                        }
                    }
                    Err(CatalogError::UniqueViolation { .. }) => {
                        assert!(conflict, "seed {seed} step {step}: spurious rejection");
                    }
                    Err(e) => panic!("seed {seed} step {step}: unexpected error {e}"),
                }
            }
            // Delete.
            _ => {
                let deleted = cat.delete("t", vec![Value::I64(id)]).unwrap();
                assert_eq!(
                    deleted,
                    model.remove(&id).is_some(),
                    "seed {seed} step {step}: delete disagreed with the model"
                );
            }
        }

        if step % 40 == 0 {
            check(&cat, &model, seed, step);
        }
    }
    check(&cat, &model, seed, STEPS);
}

/// Base table matches the model; every index matches a brute-force base scan.
fn check(cat: &Catalog<MemoryBackend>, model: &Model, seed: u64, step: u32) {
    let snap = cat.snapshot();
    // validate() IS the base↔index brute-force cross-check (plus tree
    // structural validation); the model comparison proves the base itself.
    snap.validate()
        .unwrap_or_else(|e| panic!("seed {seed} step {step}: validate: {e}"));
    let rows = snap.scan("t").unwrap();
    assert_eq!(rows.len(), model.len(), "seed {seed} step {step}");
    for row in rows {
        let Value::I64(id) = row[0] else {
            panic!("bad id")
        };
        let (email, city, age) = model.get(&id).expect("row not in model");
        let want_email = email
            .as_ref()
            .map_or(Value::Null, |e| Value::Text(e.clone()));
        assert_eq!(row[1], want_email, "seed {seed} step {step} id {id}");
        assert_eq!(row[2], Value::Text(city.clone()), "seed {seed} step {step}");
        assert_eq!(row[3], Value::I64(*age), "seed {seed} step {step}");
    }
}

#[test]
fn random_dml_keeps_every_index_consistent() {
    for seed in SEEDS {
        run(seed);
    }
}
