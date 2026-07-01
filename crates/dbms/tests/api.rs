//! Phase 10 public-API acceptance tests:
//! - **open → write → reopen → read** round-trips the committed state (scenario
//!   1, exercised through the file-backed public API);
//! - **integrity-check** flags an intentionally corrupted file;
//! - a **snapshot-owning cursor** pages a stable view while a concurrent writer
//!   inserts/updates/deletes (scenario 4 — cross-page stability).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Barrier};
use std::thread;

use otf_dbms::{
    ColumnDef, Database, Delete, Dir, ErrorCategory, Expr, Insert, Request, Select, Selector,
    SortKey, Stage, TableDef, TableRef, TypeKind, Update, Value,
};

fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "otf-dbms-{}-{}-{}.db",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

fn users_table() -> TableDef {
    TableDef::new(
        "users",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("name", TypeKind::Text).not_null(),
            ColumnDef::new("age", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    )
}

fn insert_user(db: &Database<impl common::IoBackend + 'static>, id: i64, name: &str, age: i64) {
    db.execute(&Request::Insert(Insert {
        table: "users".into(),
        rows: vec![vec![
            ("id".into(), Value::I64(id)),
            ("name".into(), Value::Text(name.into())),
            ("age".into(), Value::I64(age)),
        ]],
    }))
    .unwrap();
}

fn scan(table: &str) -> Request {
    Request::Select(Select::Pipeline(vec![Stage::Scan(TableRef {
        table: table.into(),
        alias: None,
    })]))
}

// --- scenario 1: open → write → reopen → read --------------------------------

#[cfg(unix)]
#[test]
fn crud_survives_reopen() {
    let path = temp_path("reopen");

    {
        let db = Database::create(&path).unwrap();
        db.create_table(users_table()).unwrap();
        insert_user(&db, 1, "Ada", 36);
        insert_user(&db, 2, "Grace", 45);
        insert_user(&db, 3, "Edsger", 52);

        // Update one, delete another.
        db.execute(&Request::Update(Update {
            table: "users".into(),
            selector: Some(Selector::Where(Expr::Cmp {
                op: otf_dbms::CmpOp::Eq,
                lhs: Box::new(Expr::Column {
                    table: None,
                    column: "id".into(),
                }),
                rhs: Box::new(Expr::Literal(Value::I64(1))),
            })),
            set: vec![("age".into(), Expr::Literal(Value::I64(37)))],
            unconditional: false,
        }))
        .unwrap();
        db.execute(&Request::Delete(Delete {
            table: "users".into(),
            selector: Some(Selector::Where(Expr::Cmp {
                op: otf_dbms::CmpOp::Eq,
                lhs: Box::new(Expr::Column {
                    table: None,
                    column: "id".into(),
                }),
                rhs: Box::new(Expr::Literal(Value::I64(2))),
            })),
        }))
        .unwrap();

        db.close();
    }

    // Reopen: state must be exactly what was committed.
    let db = Database::open(&path).unwrap();
    let out = db.execute(&scan("users")).unwrap();
    let mut seen: Vec<(i64, i64)> = out
        .rows()
        .map(|r| {
            (
                r.get_i64("id").unwrap().unwrap(),
                r.get_i64("age").unwrap().unwrap(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(seen, vec![(1, 37), (3, 52)]);

    // A healthy database passes its integrity check.
    let report = db.check().unwrap();
    assert_eq!(report.tables_checked, 1);

    db.close();
    let _ = std::fs::remove_file(&path);
}

// --- integrity-check detects a corrupted file --------------------------------

#[cfg(unix)]
#[test]
fn integrity_check_flags_corruption() {
    const PAGE_SIZE: usize = 4096;
    let path = temp_path("corrupt");

    {
        let db = Database::create(&path).unwrap();
        db.create_table(users_table()).unwrap();
        for id in 1..=5 {
            insert_user(&db, id, "x", id);
        }
        db.check().unwrap(); // healthy before corruption
        db.close();
    }

    // Flip a byte in every non-meta page (pages 0 and 1 are the meta slots, and
    // must stay valid so the file still opens). This is guaranteed to hit a
    // live tree or free-list page, whichever the checker reads first.
    let mut bytes = std::fs::read(&path).unwrap();
    let pages = bytes.len() / PAGE_SIZE;
    for page in 2..pages {
        bytes[page * PAGE_SIZE + 100] ^= 0xFF;
    }
    std::fs::write(&path, &bytes).unwrap();

    // Open still succeeds (it only reads the meta), but the integrity check
    // must surface a Corruption-category error.
    let db = Database::open(&path).unwrap();
    let err = db.check().unwrap_err();
    assert_eq!(
        err.category(),
        ErrorCategory::Corruption,
        "a corrupted page must be caught as Corruption, got: {err}"
    );

    db.close();
    let _ = std::fs::remove_file(&path);
}

// --- scenario 4: a cursor owns its snapshot ----------------------------------

fn items_table() -> TableDef {
    TableDef::new(
        "items",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("v", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    )
}

fn ordered_by_id() -> Select {
    Select::Pipeline(vec![
        Stage::Scan(TableRef {
            table: "items".into(),
            alias: None,
        }),
        Stage::Sort(vec![SortKey {
            expr: Expr::Column {
                table: None,
                column: "id".into(),
            },
            dir: Dir::Asc,
        }]),
    ])
}

#[test]
fn cursor_pages_a_stable_snapshot_under_a_concurrent_writer() {
    let db = Database::create_memory().unwrap();
    db.create_table(items_table()).unwrap();
    for id in 1..=50 {
        db.execute(&Request::Insert(Insert {
            table: "items".into(),
            rows: vec![vec![
                ("id".into(), Value::I64(id)),
                ("v".into(), Value::I64(0)),
            ]],
        }))
        .unwrap();
    }

    // Open the cursor first: it pins the 50-row snapshot for its whole life.
    let mut cursor = db.open_cursor(ordered_by_id(), 7).unwrap();

    // A writer hammers the table while we page: new rows, deletes, updates.
    let writer_db = db.clone();
    let barrier = Arc::new(Barrier::new(2));
    let go = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        go.wait();
        for id in 51..=120 {
            writer_db
                .execute(&Request::Insert(Insert {
                    table: "items".into(),
                    rows: vec![vec![
                        ("id".into(), Value::I64(id)),
                        ("v".into(), Value::I64(id)),
                    ]],
                }))
                .unwrap();
            // Delete an original row and bump another's non-key column.
            let victim = (id % 50) + 1;
            writer_db
                .execute(&Request::Delete(Delete {
                    table: "items".into(),
                    selector: Some(Selector::Where(eq_id(victim))),
                }))
                .unwrap();
            writer_db
                .execute(&Request::Update(Update {
                    table: "items".into(),
                    selector: Some(Selector::Where(eq_id(((id + 7) % 50) + 1))),
                    set: vec![("v".into(), Expr::Literal(Value::I64(id)))],
                    unconditional: false,
                }))
                .ok();
        }
    });

    barrier.wait();
    let mut seen: Vec<i64> = Vec::new();
    loop {
        let page = cursor.fetch().unwrap();
        for row in page.rows() {
            seen.push(row.get_i64("id").unwrap().unwrap());
        }
        if page.cursor().is_none() {
            break;
        }
    }
    writer.join().unwrap();

    // The cursor's snapshot is exactly the 50 rows that existed at open — no
    // row the concurrent writer inserted, deleted, or updated changed the walk.
    assert_eq!(seen, (1..=50).collect::<Vec<_>>());
}

fn eq_id(id: i64) -> Expr {
    Expr::Cmp {
        op: otf_dbms::CmpOp::Eq,
        lhs: Box::new(Expr::Column {
            table: None,
            column: "id".into(),
        }),
        rhs: Box::new(Expr::Literal(Value::I64(id))),
    }
}
