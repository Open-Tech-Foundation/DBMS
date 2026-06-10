//! Constraint-enforcement exit criteria: one violation test for every
//! `SPEC.md` §4.1 constraint, plus transaction atomicity for multi-row
//! inserts.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use catalog::{Catalog, CatalogError, CheckExpr, CmpOp, ColumnDef, TableDef};
use common::{ManualClock, MemoryBackend, SeededRng};
use types::{TypeKind, Value};

fn new_catalog() -> Catalog<MemoryBackend> {
    Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_700_000_000_000_000)),
        Arc::new(SeededRng::new(7)),
    )
    .unwrap()
}

/// One table exercising every constraint kind.
fn strict_table() -> TableDef {
    TableDef::new(
        "rows",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("email", TypeKind::Text).unique(),
            ColumnDef::new("name", TypeKind::Text).not_null(),
            ColumnDef::new("age", TypeKind::I64),
            ColumnDef::new("doc", TypeKind::Json),
        ],
        vec!["id"],
    )
    .check(CheckExpr::Cmp {
        col: "age".into(),
        op: CmpOp::Gte,
        value: Value::I64(0),
    })
}

fn row(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn base_row(id: i64) -> Vec<(String, Value)> {
    row(&[
        ("id", Value::I64(id)),
        ("email", Value::Text(format!("u{id}@x"))),
        ("name", Value::Text("ada".into())),
        ("age", Value::I64(30)),
    ])
}

#[test]
fn every_constraint_rejects_its_violation() {
    let cat = new_catalog();
    cat.create_table(strict_table()).unwrap();
    cat.insert("rows", base_row(1)).unwrap();

    // PRIMARY KEY: duplicate key.
    assert!(matches!(
        cat.insert("rows", base_row(1)),
        Err(CatalogError::DuplicateKey { .. })
    ));
    // PRIMARY KEY: implied NOT NULL (omitted PK without default).
    let mut no_id = base_row(2);
    no_id.retain(|(k, _)| k != "id");
    assert!(matches!(
        cat.insert("rows", no_id),
        Err(CatalogError::NotNull { .. })
    ));
    // NOT NULL: explicit null.
    let mut null_name = base_row(2);
    null_name.iter_mut().for_each(|(k, v)| {
        if k == "name" {
            *v = Value::Null;
        }
    });
    assert!(matches!(
        cat.insert("rows", null_name),
        Err(CatalogError::NotNull { .. })
    ));
    // NOT NULL: omitted without default.
    let mut no_name = base_row(2);
    no_name.retain(|(k, _)| k != "name");
    assert!(matches!(
        cat.insert("rows", no_name),
        Err(CatalogError::NotNull { .. })
    ));
    // UNIQUE: same email as row 1.
    let mut dup_email = base_row(2);
    dup_email.iter_mut().for_each(|(k, v)| {
        if k == "email" {
            *v = Value::Text("u1@x".into());
        }
    });
    assert!(matches!(
        cat.insert("rows", dup_email),
        Err(CatalogError::UniqueViolation { .. })
    ));
    // CHECK: negative age.
    let mut negative = base_row(2);
    negative.iter_mut().for_each(|(k, v)| {
        if k == "age" {
            *v = Value::I64(-1);
        }
    });
    assert!(matches!(
        cat.insert("rows", negative),
        Err(CatalogError::CheckViolation { .. })
    ));
    // Strict typing: text into an i64 column.
    let mut mistyped = base_row(2);
    mistyped.iter_mut().for_each(|(k, v)| {
        if k == "age" {
            *v = Value::Text("thirty".into());
        }
    });
    assert!(matches!(
        cat.insert("rows", mistyped),
        Err(CatalogError::TypeMismatch { .. })
    ));
    // Unknown column.
    let mut unknown = base_row(2);
    unknown.push(("ghost".into(), Value::I64(1)));
    assert!(matches!(
        cat.insert("rows", unknown),
        Err(CatalogError::UnknownColumn { .. })
    ));
    // Column provided twice.
    let mut twice = base_row(2);
    twice.push(("age".into(), Value::I64(31)));
    assert!(matches!(
        cat.insert("rows", twice),
        Err(CatalogError::DuplicateColumn { .. })
    ));
    // Malformed json document.
    let mut bad_doc = base_row(2);
    bad_doc.push(("doc".into(), Value::Json(vec![0xC1])));
    assert!(matches!(
        cat.insert("rows", bad_doc),
        Err(CatalogError::Type(_))
    ));

    // After all those rejections, exactly the one good row remains.
    assert_eq!(cat.snapshot().scan("rows").unwrap().len(), 1);
}

#[test]
fn check_follows_three_valued_logic() {
    let cat = new_catalog();
    cat.create_table(strict_table()).unwrap();

    // age NULL → the check is unknown → passes (SQL semantics).
    let mut null_age = base_row(5);
    null_age.iter_mut().for_each(|(k, v)| {
        if k == "age" {
            *v = Value::Null;
        }
    });
    cat.insert("rows", null_age).unwrap();
    assert_eq!(
        cat.snapshot()
            .get("rows", &[Value::I64(5)])
            .unwrap()
            .unwrap()[3],
        Value::Null
    );
}

#[test]
fn unique_allows_multiple_nulls() {
    let cat = new_catalog();
    cat.create_table(strict_table()).unwrap();
    for id in 1..=3 {
        let mut r = base_row(id);
        r.iter_mut().for_each(|(k, v)| {
            if k == "email" {
                *v = Value::Null;
            }
        });
        cat.insert("rows", r).unwrap();
    }
    assert_eq!(cat.snapshot().scan("rows").unwrap().len(), 3);
}

#[test]
fn multi_row_insert_is_atomic() {
    let cat = new_catalog();
    cat.create_table(strict_table()).unwrap();

    // Third row violates the check; rows one and two must not survive.
    let mut bad = base_row(3);
    bad.iter_mut().for_each(|(k, v)| {
        if k == "age" {
            *v = Value::I64(-7);
        }
    });
    let result = cat.insert_many("rows", vec![base_row(1), base_row(2), bad]);
    assert!(matches!(result, Err(CatalogError::CheckViolation { .. })));
    assert_eq!(
        cat.snapshot().scan("rows").unwrap().len(),
        0,
        "partial insert leaked"
    );

    // In-batch duplicates (PK and UNIQUE) are caught against staged rows too.
    let result = cat.insert_many("rows", vec![base_row(1), base_row(1)]);
    assert!(matches!(result, Err(CatalogError::DuplicateKey { .. })));
    let mut same_email = base_row(2);
    same_email.iter_mut().for_each(|(k, v)| {
        if k == "email" {
            *v = Value::Text("u1@x".into());
        }
    });
    let result = cat.insert_many("rows", vec![base_row(1), same_email]);
    assert!(matches!(result, Err(CatalogError::UniqueViolation { .. })));
    assert_eq!(cat.snapshot().scan("rows").unwrap().len(), 0);

    // A valid batch lands whole.
    cat.insert_many("rows", vec![base_row(1), base_row(2), base_row(3)])
        .unwrap();
    assert_eq!(cat.snapshot().scan("rows").unwrap().len(), 3);
}

#[test]
fn updates_enforce_constraints_too() {
    let cat = new_catalog();
    cat.create_table(strict_table()).unwrap();
    cat.insert("rows", base_row(1)).unwrap();
    cat.insert("rows", base_row(2)).unwrap();

    let pk = vec![Value::I64(2)];
    // CHECK on the updated row.
    assert!(matches!(
        cat.update("rows", pk.clone(), row(&[("age", Value::I64(-3))])),
        Err(CatalogError::CheckViolation { .. })
    ));
    // NOT NULL.
    assert!(matches!(
        cat.update("rows", pk.clone(), row(&[("name", Value::Null)])),
        Err(CatalogError::NotNull { .. })
    ));
    // UNIQUE against the other row.
    assert!(matches!(
        cat.update(
            "rows",
            pk.clone(),
            row(&[("email", Value::Text("u1@x".into()))])
        ),
        Err(CatalogError::UniqueViolation { .. })
    ));
    // Setting a unique column to its own current value is fine (self excluded).
    cat.update(
        "rows",
        pk.clone(),
        row(&[("email", Value::Text("u2@x".into()))]),
    )
    .unwrap();
    // Type mismatch.
    assert!(matches!(
        cat.update("rows", pk.clone(), row(&[("age", Value::Bool(true))])),
        Err(CatalogError::TypeMismatch { .. })
    ));
    // PK columns are immutable.
    assert!(matches!(
        cat.update("rows", pk.clone(), row(&[("id", Value::I64(9))])),
        Err(CatalogError::PkImmutable { .. })
    ));
    // Updating a missing row is NotFound.
    assert!(matches!(
        cat.update("rows", vec![Value::I64(99)], row(&[("age", Value::I64(1))])),
        Err(CatalogError::RowNotFound { .. })
    ));
    // A successful update lands.
    let updated = cat
        .update("rows", pk, row(&[("age", Value::I64(31))]))
        .unwrap();
    assert_eq!(updated[3], Value::I64(31));
}

#[test]
fn delete_by_pk() {
    let cat = new_catalog();
    cat.create_table(strict_table()).unwrap();
    cat.insert("rows", base_row(1)).unwrap();

    assert!(cat.delete("rows", vec![Value::I64(1)]).unwrap());
    assert!(!cat.delete("rows", vec![Value::I64(1)]).unwrap());
    assert_eq!(cat.snapshot().scan("rows").unwrap().len(), 0);
}
