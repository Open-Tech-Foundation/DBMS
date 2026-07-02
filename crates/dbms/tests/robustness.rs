//! Production-hardening coverage for the public API: the error taxonomy is
//! reported faithfully, the wire entry point never panics on hostile input,
//! transactions are all-or-nothing, and a file round-trips through reopen with
//! its integrity intact.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use otf_dbms::{
    CheckCmpOp, CheckExpr, ColumnDef, Database, Delete, ErrorCategory, Expr, IndexDef, Insert,
    Request, Selector, TableDef, TypeKind, Value,
};

fn row(cols: &[(&str, Value)]) -> Vec<(String, Value)> {
    cols.iter()
        .map(|(c, v)| (c.to_string(), v.clone()))
        .collect()
}

fn insert(
    db: &Database<impl otf_dbms::IoBackend + 'static>,
    table: &str,
    cols: &[(&str, Value)],
) -> otf_dbms::Result<()> {
    db.execute(&Request::Insert(Insert {
        table: table.into(),
        rows: vec![row(cols)],
    }))
    .map(|_| ())
}

/// A table exercising every constraint: PK, a UNIQUE column, a NOT NULL column,
/// and a CHECK.
fn constrained_db() -> Database<otf_dbms::MemoryBackend> {
    let db = Database::create_memory().unwrap();
    db.create_table(
        TableDef::new(
            "acct",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("email", TypeKind::Text).not_null().unique(),
                ColumnDef::new("balance", TypeKind::I64).not_null(),
            ],
            vec!["id"],
        )
        .check(CheckExpr::Cmp {
            col: "balance".into(),
            op: CheckCmpOp::Gte,
            value: Value::I64(0),
        }),
    )
    .unwrap();
    db
}

// --- the error taxonomy is reported faithfully -------------------------------

#[test]
fn constraint_violations_are_constraint_category() {
    let db = constrained_db();
    insert(
        &db,
        "acct",
        &[
            ("id", Value::I64(1)),
            ("email", Value::Text("a@x".into())),
            ("balance", Value::I64(10)),
        ],
    )
    .unwrap();

    // Duplicate primary key.
    let dup_pk = insert(
        &db,
        "acct",
        &[
            ("id", Value::I64(1)),
            ("email", Value::Text("b@x".into())),
            ("balance", Value::I64(5)),
        ],
    )
    .unwrap_err();
    assert_eq!(
        dup_pk.category(),
        ErrorCategory::Constraint,
        "dup pk: {dup_pk}"
    );

    // Duplicate UNIQUE value.
    let dup_unique = insert(
        &db,
        "acct",
        &[
            ("id", Value::I64(2)),
            ("email", Value::Text("a@x".into())),
            ("balance", Value::I64(5)),
        ],
    )
    .unwrap_err();
    assert_eq!(
        dup_unique.category(),
        ErrorCategory::Constraint,
        "dup unique: {dup_unique}"
    );

    // NOT NULL: omit the required email.
    let not_null = insert(
        &db,
        "acct",
        &[("id", Value::I64(3)), ("balance", Value::I64(5))],
    )
    .unwrap_err();
    assert_eq!(
        not_null.category(),
        ErrorCategory::Constraint,
        "not null: {not_null}"
    );

    // CHECK: a negative balance.
    let check = insert(
        &db,
        "acct",
        &[
            ("id", Value::I64(4)),
            ("email", Value::Text("c@x".into())),
            ("balance", Value::I64(-1)),
        ],
    )
    .unwrap_err();
    assert_eq!(
        check.category(),
        ErrorCategory::Constraint,
        "check: {check}"
    );
}

#[test]
fn malformed_requests_are_validation_category() {
    let db = constrained_db();

    // Unknown column.
    let unknown_col = insert(
        &db,
        "acct",
        &[("id", Value::I64(1)), ("nope", Value::I64(0))],
    )
    .unwrap_err();
    assert_eq!(
        unknown_col.category(),
        ErrorCategory::Validation,
        "unknown col: {unknown_col}"
    );

    // Type mismatch: text into an i64 column.
    let type_mismatch = insert(
        &db,
        "acct",
        &[
            ("id", Value::Text("x".into())),
            ("email", Value::Text("a@x".into())),
            ("balance", Value::I64(0)),
        ],
    )
    .unwrap_err();
    assert_eq!(
        type_mismatch.category(),
        ErrorCategory::Validation,
        "type: {type_mismatch}"
    );

    // A selector-less delete (SPEC §6 rule 1).
    let blind_delete = db
        .execute(&Request::Delete(Delete {
            table: "acct".into(),
            selector: None,
        }))
        .unwrap_err();
    assert_eq!(
        blind_delete.category(),
        ErrorCategory::Validation,
        "blind delete: {blind_delete}"
    );

    // A request against a table that does not exist.
    let no_table = db
        .execute(&Request::Delete(Delete {
            table: "ghost".into(),
            selector: Some(Selector::All),
        }))
        .unwrap_err();
    assert!(
        matches!(
            no_table.category(),
            ErrorCategory::Validation | ErrorCategory::NotFound
        ),
        "missing table category: {:?} ({no_table})",
        no_table.category()
    );
}

// --- the wire entry point never panics on hostile input ----------------------

#[test]
fn execute_wire_never_panics_on_garbage() {
    let db = constrained_db();
    let hostile: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x00],
        vec![0xc1],                         // the reserved MessagePack byte
        vec![0xff; 4096],                   // junk
        vec![0x81, 0xa2, b'o', b'p'],       // a truncated map
        vec![0xdd, 0xff, 0xff, 0xff, 0xff], // an array claiming 4B elements
        b"not messagepack at all".to_vec(),
    ];
    for bytes in hostile {
        // Must return a well-formed (non-empty) error result, never panic.
        let out = db.execute_wire(&bytes);
        assert!(!out.is_empty(), "empty response for input {bytes:?}");
    }
}

// --- transactions are all-or-nothing -----------------------------------------

#[test]
fn a_failing_transaction_leaves_no_partial_state() {
    let db = constrained_db();
    insert(
        &db,
        "acct",
        &[
            ("id", Value::I64(1)),
            ("email", Value::Text("a@x".into())),
            ("balance", Value::I64(10)),
        ],
    )
    .unwrap();

    // A batch whose third op violates the unique constraint (email a@x again):
    // the whole transaction must be rejected, leaving only the original row.
    let result = db.transaction(vec![
        Request::Insert(Insert {
            table: "acct".into(),
            rows: vec![row(&[
                ("id", Value::I64(2)),
                ("email", Value::Text("b@x".into())),
                ("balance", Value::I64(1)),
            ])],
        }),
        Request::Insert(Insert {
            table: "acct".into(),
            rows: vec![row(&[
                ("id", Value::I64(3)),
                ("email", Value::Text("c@x".into())),
                ("balance", Value::I64(1)),
            ])],
        }),
        Request::Insert(Insert {
            table: "acct".into(),
            rows: vec![row(&[
                ("id", Value::I64(4)),
                ("email", Value::Text("a@x".into())), // duplicate → rejects
                ("balance", Value::I64(1)),
            ])],
        }),
    ]);
    assert!(result.is_err(), "the transaction should have been rejected");

    // Only the original row survives — ids 2 and 3 were rolled back.
    let scan = db
        .execute(&Request::Select(otf_dbms::Select::Pipeline(vec![
            otf_dbms::Stage::Scan(otf_dbms::TableRef {
                table: "acct".into(),
                alias: None,
            }),
        ])))
        .unwrap();
    let mut ids: Vec<i64> = scan
        .rows()
        .map(|r| r.get_i64("id").unwrap().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1], "partial transaction leaked rows: {ids:?}");
    db.check().unwrap();
}

// --- a file round-trips through reopen with integrity intact ------------------

#[cfg(unix)]
#[test]
fn a_file_round_trips_through_reopen_with_integrity() {
    let path = std::env::temp_dir().join(format!(
        "otf-dbms-roundtrip-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    {
        let db = Database::create(&path).unwrap();
        db.create_table(TableDef::new(
            "kv",
            vec![
                ColumnDef::new("k", TypeKind::I64),
                ColumnDef::new("v", TypeKind::I64).not_null(),
            ],
            vec!["k"],
        ))
        .unwrap();
        db.create_index("kv", IndexDef::new("by_v", vec!["v"]))
            .unwrap();
        for k in 0..100 {
            insert(&db, "kv", &[("k", Value::I64(k)), ("v", Value::I64(k % 7))]).unwrap();
        }
        // Delete a swathe and update another, exercising index maintenance.
        db.execute(&Request::Delete(Delete {
            table: "kv".into(),
            selector: Some(Selector::Where(Expr::Cmp {
                op: otf_dbms::CmpOp::Lt,
                lhs: Box::new(Expr::Column {
                    table: None,
                    column: "k".into(),
                }),
                rhs: Box::new(Expr::Literal(Value::I64(10))),
            })),
        }))
        .unwrap();
        db.check().unwrap();
        db.close();
    }

    // Reopen: integrity holds and the row count is exactly what we committed.
    let db = Database::open(&path).unwrap();
    let report = db.check().unwrap();
    assert_eq!(report.tables_checked, 1);
    let count = db
        .execute(&Request::Select(otf_dbms::Select::Pipeline(vec![
            otf_dbms::Stage::Scan(otf_dbms::TableRef {
                table: "kv".into(),
                alias: None,
            }),
        ])))
        .unwrap()
        .len();
    assert_eq!(count, 90, "expected 90 rows after deleting k<10");
    db.close();
    let _ = std::fs::remove_file(&path);
}
