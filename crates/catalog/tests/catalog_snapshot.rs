//! A pinned snapshot sees one consistent version of schema *and* data,
//! no matter what DDL/DML lands after it — the payoff of hanging everything
//! off a single published root.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use catalog::{Catalog, CatalogError, ColumnDef, TableDef};
use common::{ManualClock, MemoryBackend, SeededRng};
use types::{TypeKind, Value};

fn new_catalog() -> Catalog<MemoryBackend> {
    Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_700_000_000_000_000)),
        Arc::new(SeededRng::new(3)),
    )
    .unwrap()
}

fn table(name: &str) -> TableDef {
    TableDef::new(
        name,
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("v", TypeKind::Text),
        ],
        vec!["id"],
    )
}

fn kv(id: i64, v: &str) -> Vec<(String, Value)> {
    vec![
        ("id".to_string(), Value::I64(id)),
        ("v".to_string(), Value::Text(v.into())),
    ]
}

#[test]
fn a_snapshot_pins_schema_and_data_together() {
    let cat = new_catalog();
    cat.create_table(table("a")).unwrap();
    cat.insert("a", kv(1, "old")).unwrap();

    let before = cat.snapshot();

    // Everything changes after the pin: data, schema, table set.
    cat.update("a", vec![Value::I64(1)], kv(1, "new")[1..].to_vec())
        .unwrap();
    cat.insert("a", kv(2, "extra")).unwrap();
    cat.add_column("a", ColumnDef::new("w", TypeKind::I64))
        .unwrap();
    cat.create_table(table("b")).unwrap();
    cat.drop_table("b").unwrap();
    cat.create_table(table("c")).unwrap();

    // The snapshot still reads the world as it was.
    assert_eq!(before.tables().unwrap(), vec!["a"]);
    assert_eq!(before.table("a").unwrap().columns.len(), 2);
    assert_eq!(
        before.scan("a").unwrap(),
        vec![vec![Value::I64(1), Value::Text("old".into())]]
    );
    assert!(matches!(
        before.table("c"),
        Err(CatalogError::UnknownTable { .. })
    ));

    // A fresh snapshot sees the new world.
    let after = cat.snapshot();
    assert_eq!(after.tables().unwrap(), vec!["a", "c"]);
    assert_eq!(after.table("a").unwrap().columns.len(), 3);
    assert_eq!(after.scan("a").unwrap().len(), 2);
    assert!(after.txn_id() > before.txn_id());
}

#[test]
fn many_tables_stay_independent() {
    let cat = new_catalog();
    for name in ["x", "y", "z"] {
        cat.create_table(table(name)).unwrap();
        for i in 0..20 {
            cat.insert(name, kv(i, name)).unwrap();
        }
    }
    cat.delete("y", vec![Value::I64(0)]).unwrap();

    let snap = cat.snapshot();
    assert_eq!(snap.scan("x").unwrap().len(), 20);
    assert_eq!(snap.scan("y").unwrap().len(), 19);
    assert_eq!(snap.scan("z").unwrap().len(), 20);
    assert_eq!(
        snap.get("z", &[Value::I64(7)]).unwrap().unwrap()[1],
        Value::Text("z".into())
    );
    cat.validate().unwrap();
}
