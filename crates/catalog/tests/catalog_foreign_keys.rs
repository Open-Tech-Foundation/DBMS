//! Foreign-key exit criteria: DDL validation, referencing-side enforcement
//! (child insert/update), referenced-side RESTRICT (parent delete/update and
//! `drop table`), self-referential keys, composite and unique-target keys, and
//! persistence across reopen.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use catalog::{Catalog, CatalogError, ColumnDef, ForeignKey, IndexDef, RefAction, TableDef};
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
fn on_update_cascade_cannot_move_a_primary_key() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    // `author` is part of the PK, so an on_update CASCADE would move the row's
    // key — rejected at DDL (primary keys are immutable in v1).
    let bad = TableDef::new(
        "posts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("author", TypeKind::I64),
        ],
        vec!["id", "author"],
    )
    .foreign_key(
        ForeignKey::new("fk", vec!["author"], "users", vec!["id"]).on_update(RefAction::Cascade),
    );
    let err = cat.create_table(bad).unwrap_err();
    assert!(matches!(err, CatalogError::InvalidSchema { .. }), "{err:?}");
}

/// `posts.author` → `users.id` with `on_delete` = `action`.
fn posts_on_delete(action: RefAction) -> TableDef {
    TableDef::new(
        "posts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("author", TypeKind::I64),
        ],
        vec!["id"],
    )
    .foreign_key(
        ForeignKey::new("fk_author", vec!["author"], "users", vec!["id"]).on_delete(action),
    )
}

#[test]
fn on_delete_cascade_removes_children() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts_on_delete(RefAction::Cascade))
        .unwrap();
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
    cat.insert(
        "posts",
        row(&[("id", Value::I64(11)), ("author", Value::I64(1))]),
    )
    .unwrap();

    assert!(cat.delete("users", vec![Value::I64(1)]).unwrap());
    let snap = cat.snapshot();
    assert_eq!(
        snap.row_count("posts").unwrap(),
        0,
        "children cascaded away"
    );
    assert_eq!(snap.row_count("users").unwrap(), 0);
}

#[test]
fn on_delete_set_null_nulls_children() {
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts_on_delete(RefAction::SetNull))
        .unwrap();
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

    assert!(cat.delete("users", vec![Value::I64(1)]).unwrap());
    let snap = cat.snapshot();
    let post = snap.get("posts", &[Value::I64(10)]).unwrap().unwrap();
    assert_eq!(post[1], Value::Null, "author nulled out");
    assert_eq!(snap.row_count("posts").unwrap(), 1, "child survives");
}

#[test]
fn cascade_recurses_through_a_chain() {
    // users <- posts <- comments, both edges CASCADE.
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts_on_delete(RefAction::Cascade))
        .unwrap();
    cat.create_table(
        TableDef::new(
            "comments",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("post", TypeKind::I64),
            ],
            vec!["id"],
        )
        .foreign_key(
            ForeignKey::new("fk_post", vec!["post"], "posts", vec!["id"])
                .on_delete(RefAction::Cascade),
        ),
    )
    .unwrap();

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
    cat.insert(
        "comments",
        row(&[("id", Value::I64(100)), ("post", Value::I64(10))]),
    )
    .unwrap();

    assert!(cat.delete("users", vec![Value::I64(1)]).unwrap());
    let snap = cat.snapshot();
    assert_eq!(snap.row_count("posts").unwrap(), 0);
    assert_eq!(
        snap.row_count("comments").unwrap(),
        0,
        "grandchild cascaded away"
    );
}

#[test]
fn a_downstream_restrict_blocks_a_cascade() {
    // users <-CASCADE- posts <-RESTRICT- comments. Deleting the user would
    // cascade-delete the post, but a comment restricts the post → whole delete
    // is rejected and nothing changes.
    let (cat, _b) = new_catalog();
    cat.create_table(users()).unwrap();
    cat.create_table(posts_on_delete(RefAction::Cascade))
        .unwrap();
    cat.create_table(
        TableDef::new(
            "comments",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("post", TypeKind::I64),
            ],
            vec!["id"],
        )
        .foreign_key(ForeignKey::new(
            "fk_post",
            vec!["post"],
            "posts",
            vec!["id"],
        )),
    )
    .unwrap();

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
    cat.insert(
        "comments",
        row(&[("id", Value::I64(100)), ("post", Value::I64(10))]),
    )
    .unwrap();

    let err = cat.delete("users", vec![Value::I64(1)]).unwrap_err();
    assert!(
        matches!(err, CatalogError::ReferencedByChildren { .. }),
        "{err:?}"
    );
    // The whole operation was a no-op.
    let snap = cat.snapshot();
    assert_eq!(snap.row_count("users").unwrap(), 1);
    assert_eq!(snap.row_count("posts").unwrap(), 1);
    assert_eq!(snap.row_count("comments").unwrap(), 1);
}

#[test]
fn self_referential_cascade_deletes_the_subtree() {
    let (cat, _b) = new_catalog();
    cat.create_table(
        TableDef::new(
            "nodes",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("parent", TypeKind::I64),
            ],
            vec!["id"],
        )
        .foreign_key(
            ForeignKey::new("fk_parent", vec!["parent"], "nodes", vec!["id"])
                .on_delete(RefAction::Cascade),
        ),
    )
    .unwrap();
    cat.insert(
        "nodes",
        row(&[("id", Value::I64(1)), ("parent", Value::Null)]),
    )
    .unwrap();
    cat.insert(
        "nodes",
        row(&[("id", Value::I64(2)), ("parent", Value::I64(1))]),
    )
    .unwrap();
    cat.insert(
        "nodes",
        row(&[("id", Value::I64(3)), ("parent", Value::I64(2))]),
    )
    .unwrap();

    assert!(cat.delete("nodes", vec![Value::I64(1)]).unwrap());
    assert_eq!(
        cat.snapshot().row_count("nodes").unwrap(),
        0,
        "subtree gone"
    );
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

// --- index consistency across a cascade --------------------------------------

/// `posts` with both a unique index (`slug`) and a non-unique one (`by_author`),
/// referencing `users` with the given `on_delete` action.
fn indexed_posts(action: RefAction) -> TableDef {
    TableDef::new(
        "posts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("author", TypeKind::I64),
            ColumnDef::new("slug", TypeKind::Text).unique(),
        ],
        vec!["id"],
    )
    .index(IndexDef::new("by_author", vec!["author"]))
    .foreign_key(
        ForeignKey::new("fk_author", vec!["author"], "users", vec!["id"]).on_delete(action),
    )
}

/// Create `users` then `posts` (FK requires the parent first) and seed rows.
fn seed_indexed_posts(cat: &Catalog<Backend>, action: RefAction) {
    cat.create_table(users()).unwrap();
    cat.create_table(indexed_posts(action)).unwrap();
    cat.insert(
        "users",
        row(&[("id", Value::I64(1)), ("name", Value::Text("ada".into()))]),
    )
    .unwrap();
    cat.insert(
        "users",
        row(&[("id", Value::I64(2)), ("name", Value::Text("bo".into()))]),
    )
    .unwrap();
    for (id, author, slug) in [(10, 1, "a"), (11, 1, "b"), (12, 2, "c")] {
        cat.insert(
            "posts",
            row(&[
                ("id", Value::I64(id)),
                ("author", Value::I64(author)),
                ("slug", Value::Text(slug.into())),
            ]),
        )
        .unwrap();
    }
}

#[test]
fn cascade_delete_keeps_secondary_indexes_consistent() {
    let (cat, _b) = new_catalog();
    seed_indexed_posts(&cat, RefAction::Cascade);

    assert!(cat.delete("users", vec![Value::I64(1)]).unwrap());
    let snap = cat.snapshot();
    assert_eq!(
        snap.row_count("posts").unwrap(),
        1,
        "author 2's post remains"
    );
    // Every index still matches its base table entry-for-entry.
    snap.validate().unwrap();
}

#[test]
fn set_null_keeps_secondary_indexes_consistent() {
    let (cat, _b) = new_catalog();
    seed_indexed_posts(&cat, RefAction::SetNull);

    assert!(cat.delete("users", vec![Value::I64(1)]).unwrap());
    let snap = cat.snapshot();
    assert_eq!(snap.row_count("posts").unwrap(), 3, "children survive");
    // Author of posts 10 and 11 is now NULL; the by_author index must reflect
    // that and still validate.
    assert_eq!(
        snap.get("posts", &[Value::I64(10)]).unwrap().unwrap()[1],
        Value::Null
    );
    snap.validate().unwrap();
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
fn cascade_uses_a_composite_prefix_index() {
    // The child indexes (country, region, name); the foreign key is on
    // (country, region) — a prefix of that index — so scan_children takes the
    // O(log n) range-probe path. This checks it resolves the right children.
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
                ColumnDef::new("name", TypeKind::Text),
            ],
            vec!["id"],
        )
        .index(IndexDef::new(
            "by_region",
            vec!["country", "region", "name"],
        ))
        .foreign_key(
            ForeignKey::new(
                "fk_region",
                vec!["country", "region"],
                "regions",
                vec!["country", "code"],
            )
            .on_delete(RefAction::Cascade),
        ),
    )
    .unwrap();

    for code in [5, 6] {
        cat.insert(
            "regions",
            row(&[
                ("country", Value::Text("US".into())),
                ("code", Value::I64(code)),
            ]),
        )
        .unwrap();
    }
    // Two cities in region 5, one in region 6.
    for (id, region, name) in [(1, 5, "A"), (2, 5, "B"), (3, 6, "C")] {
        cat.insert(
            "cities",
            row(&[
                ("id", Value::I64(id)),
                ("country", Value::Text("US".into())),
                ("region", Value::I64(region)),
                ("name", Value::Text(name.into())),
            ]),
        )
        .unwrap();
    }

    // Deleting region 5 cascades only its two cities; region 6's city survives.
    cat.delete("regions", vec![Value::Text("US".into()), Value::I64(5)])
        .unwrap();
    let snap = cat.snapshot();
    assert_eq!(
        snap.row_count("cities").unwrap(),
        1,
        "only the region-6 city remains"
    );
    assert!(snap.get("cities", &[Value::I64(3)]).unwrap().is_some());
    snap.validate().unwrap();
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

/// `tags` keyed by id but referenced through UNIQUE `slug`, and `hits.slug` →
/// `tags.slug` with the given `on_update` action.
fn seed_tags_and_hits(cat: &Catalog<Backend>, action: RefAction) {
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
        .foreign_key(
            ForeignKey::new("fk_slug", vec!["slug"], "tags", vec!["slug"]).on_update(action),
        ),
    )
    .unwrap();
    cat.insert(
        "tags",
        row(&[("id", Value::I64(1)), ("slug", Value::Text("rust".into()))]),
    )
    .unwrap();
    for id in [1, 2] {
        cat.insert(
            "hits",
            row(&[("id", Value::I64(id)), ("slug", Value::Text("rust".into()))]),
        )
        .unwrap();
    }
}

#[test]
fn on_update_cascade_renames_children() {
    let (cat, _b) = new_catalog();
    seed_tags_and_hits(&cat, RefAction::Cascade);

    // Rename the referenced slug; both children follow.
    cat.update(
        "tags",
        vec![Value::I64(1)],
        row(&[("slug", Value::Text("rustlang".into()))]),
    )
    .unwrap();
    let snap = cat.snapshot();
    for id in [1, 2] {
        assert_eq!(
            snap.get("hits", &[Value::I64(id)]).unwrap().unwrap()[1],
            Value::Text("rustlang".into()),
        );
    }
    snap.validate().unwrap();
}

#[test]
fn on_update_set_null_nulls_children() {
    let (cat, _b) = new_catalog();
    seed_tags_and_hits(&cat, RefAction::SetNull);

    cat.update(
        "tags",
        vec![Value::I64(1)],
        row(&[("slug", Value::Text("rustlang".into()))]),
    )
    .unwrap();
    let snap = cat.snapshot();
    for id in [1, 2] {
        assert_eq!(
            snap.get("hits", &[Value::I64(id)]).unwrap().unwrap()[1],
            Value::Null,
        );
    }
    snap.validate().unwrap();
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
