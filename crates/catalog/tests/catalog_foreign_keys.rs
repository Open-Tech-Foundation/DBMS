//! Foreign-key exit criteria: DDL validation, referencing-side enforcement
//! (child insert/update), referenced-side RESTRICT (parent delete/update and
//! `drop table`), self-referential keys, composite and unique-target keys, and
//! persistence across reopen.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use catalog::{Catalog, CatalogError, ColumnDef, ForeignKey, RefAction, TableDef};
use common::{ManualClock, MemoryBackend, SeededRng};
use types::{TypeKind, Value};

type Backend = Arc<MemoryBackend>;

fn services() -> (Arc<ManualClock>, Arc<SeededRng>) {
    (
        Arc::new(ManualClock::new(1_700_000_000_000_000)),
        Arc::new(SeededRng::new(7)),
    )
}

fn new_catalog() -> (Catalog<Backend>, Backend) {
    let backend: Backend = Arc::new(MemoryBackend::new());
    let (clock, rng) = services();
    let cat = Catalog::create(Arc::clone(&backend), clock, rng).unwrap();
    (cat, backend)
}

fn users() -> TableDef {
    TableDef::new(
        "users",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("name", TypeKind::Text).not_null(),
        ],
        vec!["id"],
    )
}

/// `posts.author` → `users.id`, nullable so MATCH SIMPLE is exercisable.
fn posts() -> TableDef {
    TableDef::new(
        "posts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("author", TypeKind::I64),
        ],
        vec!["id"],
    )
    .foreign_key(ForeignKey::new(
        "fk_author",
        vec!["author"],
        "users",
        vec!["id"],
    ))
}

fn row(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

// --- DDL validation ----------------------------------------------------------

#[test]
fn fk_to_unknown_table_is_rejected() {
    let (cat, _b) = new_catalog();
    let err = cat.create_table(posts()).unwrap_err();
    assert!(matches!(err, CatalogError::InvalidSchema { .. }), "{err:?}");
}

#[test]
fn fk_must_reference_a_key_or_unique_column() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    // `users.name` is neither the primary key nor unique.
    let bad = TableDef::new(
        "posts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("who", TypeKind::Text),
        ],
        vec!["id"],
    )
    .foreign_key(ForeignKey::new("fk", vec!["who"], "users", vec!["name"]));
    let err = cat.create_table(bad).unwrap_err();
    assert!(matches!(err, CatalogError::InvalidSchema { .. }), "{err:?}");
}

#[test]
fn fk_column_type_must_match_the_parent() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    let bad = TableDef::new(
        "posts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("author", TypeKind::Text), // parent id is i64
        ],
        vec!["id"],
    )
    .foreign_key(ForeignKey::new("fk", vec!["author"], "users", vec!["id"]));
    let err = cat.create_table(bad).unwrap_err();
    assert!(matches!(err, CatalogError::InvalidSchema { .. }), "{err:?}");
}

#[test]
fn cascade_and_set_null_are_not_yet_supported() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    let cascade = posts().foreign_key(
        ForeignKey::new("fk2", vec!["author"], "users", vec!["id"]).on_delete(RefAction::Cascade),
    );
    let err = cat.create_table(cascade).unwrap_err();
    assert!(matches!(err, CatalogError::InvalidSchema { .. }), "{err:?}");
}

// --- referencing side (child insert / update) --------------------------------

#[test]
fn child_insert_requires_an_existing_parent() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts()).unwrap();
    cat.insert(
        "users",
        row(&[("id", Value::I64(1)), ("name", Value::Text("ada".into()))]),
    )
    .unwrap();

    // Existing parent → ok.
    cat.insert(
        "posts",
        row(&[("id", Value::I64(10)), ("author", Value::I64(1))]),
    )
    .unwrap();
    // Missing parent → foreign-key violation.
    let err = cat
        .insert(
            "posts",
            row(&[("id", Value::I64(11)), ("author", Value::I64(99))]),
        )
        .unwrap_err();
    assert!(
        matches!(err, CatalogError::ForeignKeyViolation { .. }),
        "{err:?}"
    );
    // NULL referencing column is not checked (MATCH SIMPLE).
    cat.insert(
        "posts",
        row(&[("id", Value::I64(12)), ("author", Value::Null)]),
    )
    .unwrap();
}

#[test]
fn child_update_to_a_missing_parent_is_rejected() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts()).unwrap();
    cat.insert(
        "users",
        row(&[("id", Value::I64(1)), ("name", Value::Text("ada".into()))]),
    )
    .unwrap();
    cat.insert(
        "posts",
        row(&[("id", Value::I64(10)), ("author", Value::I64(1))]),
    )
    .unwrap();

    let err = cat
        .update(
            "posts",
            vec![Value::I64(10)],
            row(&[("author", Value::I64(42))]),
        )
        .unwrap_err();
    assert!(
        matches!(err, CatalogError::ForeignKeyViolation { .. }),
        "{err:?}"
    );
    // Re-pointing to NULL is allowed.
    cat.update(
        "posts",
        vec![Value::I64(10)],
        row(&[("author", Value::Null)]),
    )
    .unwrap();
}

// --- referenced side (parent delete / drop) ----------------------------------

#[test]
fn parent_delete_is_restricted_while_children_exist() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts()).unwrap();
    cat.insert(
        "users",
        row(&[("id", Value::I64(1)), ("name", Value::Text("ada".into()))]),
    )
    .unwrap();
    cat.insert(
        "posts",
        row(&[("id", Value::I64(10)), ("author", Value::I64(1))]),
    )
    .unwrap();

    let err = cat.delete("users", vec![Value::I64(1)]).unwrap_err();
    assert!(
        matches!(err, CatalogError::ReferencedByChildren { .. }),
        "{err:?}"
    );

    // Remove the child, then the parent deletes cleanly.
    assert!(cat.delete("posts", vec![Value::I64(10)]).unwrap());
    assert!(cat.delete("users", vec![Value::I64(1)]).unwrap());
}

#[test]
fn dropping_a_referenced_table_is_rejected() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts()).unwrap();

    let err = cat.drop_table("users").unwrap_err();
    assert!(
        matches!(err, CatalogError::TableReferenced { .. }),
        "{err:?}"
    );

    // Dropping the child first frees the parent to be dropped.
    cat.drop_table("posts").unwrap();
    cat.drop_table("users").unwrap();
}

// --- self-referential --------------------------------------------------------

fn employees() -> TableDef {
    TableDef::new(
        "employees",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("manager", TypeKind::I64),
        ],
        vec!["id"],
    )
    .foreign_key(ForeignKey::new(
        "fk_mgr",
        vec!["manager"],
        "employees",
        vec!["id"],
    ))
}

#[test]
fn self_referential_key_enforces_and_restricts() {
    let (cat, _b) = new_catalog();
    cat.create_table(employees()).unwrap();

    cat.insert(
        "employees",
        row(&[("id", Value::I64(1)), ("manager", Value::Null)]),
    )
    .unwrap();
    cat.insert(
        "employees",
        row(&[("id", Value::I64(2)), ("manager", Value::I64(1))]),
    )
    .unwrap();
    // Manager 99 does not exist.
    let err = cat
        .insert(
            "employees",
            row(&[("id", Value::I64(3)), ("manager", Value::I64(99))]),
        )
        .unwrap_err();
    assert!(
        matches!(err, CatalogError::ForeignKeyViolation { .. }),
        "{err:?}"
    );
    // Employee 1 still manages employee 2.
    let err = cat.delete("employees", vec![Value::I64(1)]).unwrap_err();
    assert!(
        matches!(err, CatalogError::ReferencedByChildren { .. }),
        "{err:?}"
    );
    // A leaf deletes cleanly, and does not block its own removal.
    assert!(cat.delete("employees", vec![Value::I64(2)]).unwrap());
    assert!(cat.delete("employees", vec![Value::I64(1)]).unwrap());
}

// --- composite & unique-target -----------------------------------------------

#[test]
fn composite_foreign_key_is_enforced() {
    let (cat, _b) = new_catalog();
    cat.create_table(TableDef::new(
        "regions",
        vec![
            ColumnDef::new("country", TypeKind::Text),
            ColumnDef::new("code", TypeKind::I64),
        ],
        vec!["country", "code"],
    ))
    .unwrap();
    cat.create_table(
        TableDef::new(
            "cities",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("country", TypeKind::Text),
                ColumnDef::new("region", TypeKind::I64),
            ],
            vec!["id"],
        )
        .foreign_key(ForeignKey::new(
            "fk_region",
            vec!["country", "region"],
            "regions",
            vec!["country", "code"],
        )),
    )
    .unwrap();

    cat.insert(
        "regions",
        row(&[
            ("country", Value::Text("US".into())),
            ("code", Value::I64(5)),
        ]),
    )
    .unwrap();
    cat.insert(
        "cities",
        row(&[
            ("id", Value::I64(1)),
            ("country", Value::Text("US".into())),
            ("region", Value::I64(5)),
        ]),
    )
    .unwrap();
    // Right country, wrong region code.
    let err = cat
        .insert(
            "cities",
            row(&[
                ("id", Value::I64(2)),
                ("country", Value::Text("US".into())),
                ("region", Value::I64(6)),
            ]),
        )
        .unwrap_err();
    assert!(
        matches!(err, CatalogError::ForeignKeyViolation { .. }),
        "{err:?}"
    );
}

#[test]
fn changing_a_referenced_unique_key_is_restricted() {
    let (cat, _b) = new_catalog();
    // Parent keyed by id, but referenced through a UNIQUE `slug`.
    cat.create_table(TableDef::new(
        "tags",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("slug", TypeKind::Text).unique(),
        ],
        vec!["id"],
    ))
    .unwrap();
    cat.create_table(
        TableDef::new(
            "hits",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("slug", TypeKind::Text),
            ],
            vec!["id"],
        )
        .foreign_key(ForeignKey::new(
            "fk_slug",
            vec!["slug"],
            "tags",
            vec!["slug"],
        )),
    )
    .unwrap();

    cat.insert(
        "tags",
        row(&[("id", Value::I64(1)), ("slug", Value::Text("rust".into()))]),
    )
    .unwrap();
    cat.insert(
        "hits",
        row(&[("id", Value::I64(1)), ("slug", Value::Text("rust".into()))]),
    )
    .unwrap();

    // Renaming the referenced slug would orphan the child.
    let err = cat
        .update(
            "tags",
            vec![Value::I64(1)],
            row(&[("slug", Value::Text("rustlang".into()))]),
        )
        .unwrap_err();
    assert!(
        matches!(err, CatalogError::ReferencedByChildren { .. }),
        "{err:?}"
    );
}

// --- persistence -------------------------------------------------------------

#[test]
fn foreign_keys_survive_reopen() {
    let (cat, backend) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts()).unwrap();
    drop(cat);

    let (clock, rng) = services();
    let cat = Catalog::open(backend, clock, rng).unwrap();
    let def = cat.snapshot().table("posts").unwrap();
    assert_eq!(def, posts());
    assert_eq!(def.foreign_keys.len(), 1);
    assert_eq!(def.foreign_keys[0].parent, "users");
}
