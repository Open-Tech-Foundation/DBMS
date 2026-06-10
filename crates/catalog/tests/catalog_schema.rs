//! Schema persistence exit criteria: create / reopen / inspect round-trips,
//! `update: guarded` persisted and readable, and DDL behavior (drop table,
//! add column).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use catalog::{
    Catalog, CatalogError, CheckExpr, CmpOp, ColumnDef, DefaultSpec, TableDef, UpdatePolicy,
};
use common::{ManualClock, MemoryBackend, SeededRng};
use types::{TypeKind, Value};

type Backend = Arc<MemoryBackend>;

fn services() -> (Arc<ManualClock>, Arc<SeededRng>) {
    (
        Arc::new(ManualClock::new(1_700_000_000_000_000)),
        Arc::new(SeededRng::new(42)),
    )
}

fn new_catalog() -> (Catalog<Backend>, Backend) {
    let backend: Backend = Arc::new(MemoryBackend::new());
    let (clock, rng) = services();
    let cat = Catalog::create(Arc::clone(&backend), clock, rng).unwrap();
    (cat, backend)
}

/// The SPEC §4.3 example `accounts` table, almost verbatim (rowversion is
/// i64 per DECISIONS.md D13).
fn accounts() -> TableDef {
    TableDef::new(
        "accounts",
        vec![
            ColumnDef::new("id", TypeKind::Uuid).default_uuid_v7(),
            ColumnDef::new("balance", TypeKind::I64)
                .not_null()
                .guarded(),
            ColumnDef::new("version", TypeKind::I64).rowversion(),
            ColumnDef::new("created_at", TypeKind::Timestamp).default_now(),
            ColumnDef::new("updated_at", TypeKind::Timestamp).on_update_now(),
        ],
        vec!["id"],
    )
    .check(CheckExpr::Cmp {
        col: "balance".into(),
        op: CmpOp::Gte,
        value: Value::I64(0),
    })
}

#[test]
fn definitions_survive_reopen_exactly() {
    let (cat, backend) = new_catalog();
    cat.create_table(accounts()).unwrap();
    cat.create_table(TableDef::new(
        "users",
        vec![
            ColumnDef::new("id", TypeKind::I64).auto_increment(),
            ColumnDef::new("first_name", TypeKind::Text),
            ColumnDef::new("data", TypeKind::Json),
        ],
        vec!["id"],
    ))
    .unwrap();
    drop(cat);

    let (clock, rng) = services();
    let cat = Catalog::open(backend, clock, rng).unwrap();
    let snap = cat.snapshot();
    assert_eq!(snap.tables().unwrap(), vec!["accounts", "users"]);

    // Byte-for-byte definition round-trip, including policy and checks.
    let def = snap.table("accounts").unwrap();
    assert_eq!(def, accounts());
    let balance = &def.columns[def.col_index("balance").unwrap()];
    assert_eq!(balance.update, UpdatePolicy::Guarded);
    assert!(!balance.nullable);
    assert_eq!(def.checks.len(), 1);

    let users = snap.table("users").unwrap();
    assert!(users.columns[0].auto_increment);
    assert_eq!(users.columns[2].kind, TypeKind::Json);
}

#[test]
fn invalid_definitions_are_rejected() {
    let (cat, _b) = new_catalog();
    let id = || ColumnDef::new("id", TypeKind::I64);

    // No PK.
    let no_pk = TableDef::new("t", vec![id()], Vec::<String>::new());
    assert!(matches!(
        cat.create_table(no_pk),
        Err(CatalogError::InvalidSchema { .. })
    ));
    // Duplicate column.
    let dup = TableDef::new("t", vec![id(), id()], vec!["id"]);
    assert!(matches!(
        cat.create_table(dup),
        Err(CatalogError::InvalidSchema { .. })
    ));
    // PK over a missing column.
    let ghost = TableDef::new("t", vec![id()], vec!["nope"]);
    assert!(matches!(
        cat.create_table(ghost),
        Err(CatalogError::UnknownColumn { .. })
    ));
    // auto_increment on a non-i64 column.
    let bad_auto = TableDef::new(
        "t",
        vec![ColumnDef::new("id", TypeKind::Text).auto_increment()],
        vec!["id"],
    );
    assert!(matches!(
        cat.create_table(bad_auto),
        Err(CatalogError::InvalidSchema { .. })
    ));
    // json as a key.
    let json_pk = TableDef::new("t", vec![ColumnDef::new("id", TypeKind::Json)], vec!["id"]);
    assert!(matches!(
        cat.create_table(json_pk),
        Err(CatalogError::InvalidSchema { .. })
    ));
    // Check literal of the wrong kind.
    let bad_check = TableDef::new("t", vec![id()], vec!["id"]).check(CheckExpr::Cmp {
        col: "id".into(),
        op: CmpOp::Gt,
        value: Value::Text("zero".into()),
    });
    assert!(matches!(
        cat.create_table(bad_check),
        Err(CatalogError::InvalidSchema { .. })
    ));
    // Duplicate create.
    cat.create_table(TableDef::new("t", vec![id()], vec!["id"]))
        .unwrap();
    assert!(matches!(
        cat.create_table(TableDef::new("t", vec![id()], vec!["id"])),
        Err(CatalogError::TableExists { .. })
    ));
}

#[test]
fn drop_table_removes_it_and_frees_its_pages() {
    let (cat, backend) = new_catalog();
    cat.create_table(TableDef::new(
        "big",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("payload", TypeKind::Blob),
        ],
        vec!["id"],
    ))
    .unwrap();
    for i in 0..200i64 {
        cat.insert(
            "big",
            vec![
                ("id".into(), Value::I64(i)),
                ("payload".into(), Value::Blob(vec![0xAB; 200])),
            ],
        )
        .unwrap();
    }
    let before = cat.validate().unwrap();

    cat.drop_table("big").unwrap();
    assert!(matches!(
        cat.snapshot().table("big"),
        Err(CatalogError::UnknownTable { .. })
    ));
    assert!(matches!(
        cat.drop_table("big"),
        Err(CatalogError::UnknownTable { .. })
    ));

    // The table's pages return to the free list (no snapshots pinned them);
    // the next write triggers reclamation.
    cat.create_table(TableDef::new(
        "tiny",
        vec![ColumnDef::new("id", TypeKind::I64)],
        vec!["id"],
    ))
    .unwrap();
    let after = cat.validate().unwrap();
    assert!(
        after.free_ids > before.free_ids + 10,
        "dropping a 200-row table should free many pages \
         (before: {}, after: {})",
        before.free_ids,
        after.free_ids
    );

    // Still gone after reopen.
    drop(cat);
    let (clock, rng) = services();
    let cat = Catalog::open(backend, clock, rng).unwrap();
    assert_eq!(cat.snapshot().tables().unwrap(), vec!["tiny"]);
}

#[test]
fn add_column_pads_existing_rows() {
    let (cat, backend) = new_catalog();
    cat.create_table(TableDef::new(
        "t",
        vec![ColumnDef::new("id", TypeKind::I64)],
        vec!["id"],
    ))
    .unwrap();
    cat.insert("t", vec![("id".into(), Value::I64(1))]).unwrap();

    // A NOT NULL column without a constant default cannot be added.
    assert!(matches!(
        cat.add_column("t", ColumnDef::new("strict", TypeKind::Text).not_null()),
        Err(CatalogError::InvalidSchema { .. })
    ));
    // Nullable and constant-default columns can.
    cat.add_column("t", ColumnDef::new("note", TypeKind::Text))
        .unwrap();
    cat.add_column(
        "t",
        ColumnDef::new("score", TypeKind::I64)
            .not_null()
            .default_value(Value::I64(100)),
    )
    .unwrap();

    // The old row reads padded: NULL for `note`, the default for `score`.
    let row = cat.snapshot().get("t", &[Value::I64(1)]).unwrap().unwrap();
    assert_eq!(row, vec![Value::I64(1), Value::Null, Value::I64(100)]);

    // New rows carry all columns; everything survives reopen.
    cat.insert(
        "t",
        vec![
            ("id".into(), Value::I64(2)),
            ("note".into(), Value::Text("hi".into())),
        ],
    )
    .unwrap();
    drop(cat);
    let (clock, rng) = services();
    let cat = Catalog::open(backend, clock, rng).unwrap();
    let rows = cat.snapshot().scan("t").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::I64(1), Value::Null, Value::I64(100)],
            vec![Value::I64(2), Value::Text("hi".into()), Value::I64(100)],
        ]
    );

    // A duplicate column name is rejected.
    assert!(matches!(
        cat.add_column("t", ColumnDef::new("note", TypeKind::I64)),
        Err(CatalogError::InvalidSchema { .. })
    ));
}

#[test]
fn default_specs_round_trip() {
    let (cat, _b) = new_catalog();
    cat.create_table(TableDef::new(
        "d",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("flag", TypeKind::Bool).default_value(Value::Bool(true)),
            ColumnDef::new("uid", TypeKind::Uuid).default_uuid_v7(),
            ColumnDef::new("at", TypeKind::Timestamp).default_now(),
        ],
        vec!["id"],
    ))
    .unwrap();
    let def = cat.snapshot().table("d").unwrap();
    assert_eq!(
        def.columns[1].default,
        Some(DefaultSpec::Value(Value::Bool(true)))
    );
    assert_eq!(def.columns[2].default, Some(DefaultSpec::UuidV7));
    assert_eq!(def.columns[3].default, Some(DefaultSpec::Now));
}
