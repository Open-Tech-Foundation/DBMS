//! Phase 11 acceptance scenarios (`PLAN.md` §7), driven **end-to-end through
//! the public `otf_dbms` API** — the surface a real integrator uses.
//!
//! Scenarios 1 (CRUD + reopen) and 4 (keyset pagination stability) live in
//! `api.rs`; this file covers the rest that the API can express directly:
//!
//! - **2** — an indexed lookup is served by an index seek (verified via EXPLAIN)
//!   and returns the rows a scan would.
//! - **3** — an INNER join across three tables with GROUP BY + aggregates, in
//!   **both** the pipeline and clause surfaces, returns identical results.
//! - **5** — the bank scenario: two concurrent guarded relative withdrawals
//!   serialize so exactly one succeeds and the CHECK never breaks (run many
//!   times).
//! - **6** — an optimistic (version-guarded) update is first-committer-wins.
//! - **7** — guard-rule enforcement: a blind absolute set to a guarded column,
//!   and a selector-less update/delete, are both rejected.
//! - **8** — crash durability: a fault injected at every commit I/O boundary
//!   recovers to a valid database that reflects exactly the committed rows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Barrier};
use std::thread;

use common::{
    Clock, FaultInjectingBackend, FaultPoint, MemoryBackend, Rng, SeededRng, SystemClock,
};
use otf_dbms::{
    AggFunc, ArithOp, CheckCmpOp, CheckExpr, ClauseSelect, CmpOp, ColumnDef, Database, Dir,
    ErrorCategory, Expr, IndexDef, Insert, JoinKind, JoinSpec, Projection, Request, Select,
    Selector, SortKey, Stage, TableDef, TableRef, TypeKind, Update, Value,
};

// --- small expression/AST builders -------------------------------------------

fn col(name: &str) -> Expr {
    Expr::Column {
        table: None,
        column: name.into(),
    }
}

fn qcol(table: &str, name: &str) -> Expr {
    Expr::Column {
        table: Some(table.into()),
        column: name.into(),
    }
}

fn lit(n: i64) -> Expr {
    Expr::Literal(Value::I64(n))
}

fn cmp(op: CmpOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Cmp {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn tref(table: &str) -> TableRef {
    TableRef {
        table: table.into(),
        alias: None,
    }
}

fn insert(db: &Database<impl common::IoBackend + 'static>, table: &str, row: Vec<(&str, i64)>) {
    db.execute(&Request::Insert(Insert {
        table: table.into(),
        rows: vec![row
            .into_iter()
            .map(|(c, v)| (c.to_string(), Value::I64(v)))
            .collect()],
    }))
    .unwrap();
}

// --- scenario 2: indexed lookup ----------------------------------------------

#[test]
fn indexed_lookup_uses_a_seek_and_matches_a_scan() {
    let db = Database::create_memory().unwrap();
    db.create_table(TableDef::new(
        "users",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("dept", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    ))
    .unwrap();
    db.create_index("users", IndexDef::new("by_dept", vec!["dept"]))
        .unwrap();
    for id in 1..=20 {
        insert(&db, "users", vec![("id", id), ("dept", id % 4)]);
    }

    // A `WHERE dept = 2` query.
    let where_dept_2 = Select::Pipeline(vec![
        Stage::Scan(tref("users")),
        Stage::Match(cmp(CmpOp::Eq, col("dept"), lit(2))),
    ]);

    // EXPLAIN must show the planner chose an index seek on `by_dept`.
    let explain = db.execute(&Request::Explain(where_dept_2.clone())).unwrap();
    let plan_text: String = explain
        .rows()
        .map(|r| r.get_text("plan").unwrap().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plan_text.contains("IndexScan") && plan_text.contains("by_dept"),
        "expected an index seek, got plan:\n{plan_text}"
    );

    // And the seek returns exactly the rows a full scan + filter would.
    let mut via_index: Vec<i64> = db
        .execute(&Request::Select(where_dept_2))
        .unwrap()
        .rows()
        .map(|r| r.get_i64("id").unwrap().unwrap())
        .collect();
    via_index.sort();
    let expected: Vec<i64> = (1..=20).filter(|id| id % 4 == 2).collect();
    assert_eq!(via_index, expected);
}

// --- scenario 3: join + group, both surfaces ---------------------------------

/// Build the three-table join + group query in the pipeline surface:
/// employees ⋈ depts ⋈ regions, grouped by region name, counting employees.
fn three_table_pipeline() -> Select {
    Select::Pipeline(vec![
        Stage::Scan(tref("emp")),
        Stage::Join(JoinSpec {
            kind: JoinKind::Inner,
            table: tref("dept"),
            on: Some(cmp(CmpOp::Eq, qcol("emp", "dept_id"), qcol("dept", "id"))),
        }),
        Stage::Join(JoinSpec {
            kind: JoinKind::Inner,
            table: tref("region"),
            on: Some(cmp(
                CmpOp::Eq,
                qcol("dept", "region_id"),
                qcol("region", "id"),
            )),
        }),
        Stage::Group {
            by: vec![qcol("region", "id")],
            aggs: vec![(
                "headcount".into(),
                Expr::Agg {
                    func: AggFunc::Count,
                    arg: Box::new(qcol("emp", "id")),
                },
            )],
        },
        // Project drops table qualifiers, so alias the group key and sort/read
        // by that name (ORDER BY runs after SELECT in the pipeline).
        Stage::Project(vec![
            Projection::Aliased {
                name: "region_id".into(),
                expr: qcol("region", "id"),
            },
            Projection::Expr(col("headcount")),
        ]),
        Stage::Sort(vec![SortKey {
            expr: col("region_id"),
            dir: Dir::Asc,
        }]),
    ])
}

/// The same query in the clause surface.
fn three_table_clause() -> Select {
    Select::Clause(Box::new(ClauseSelect {
        from: Some(tref("emp")),
        joins: vec![
            JoinSpec {
                kind: JoinKind::Inner,
                table: tref("dept"),
                on: Some(cmp(CmpOp::Eq, qcol("emp", "dept_id"), qcol("dept", "id"))),
            },
            JoinSpec {
                kind: JoinKind::Inner,
                table: tref("region"),
                on: Some(cmp(
                    CmpOp::Eq,
                    qcol("dept", "region_id"),
                    qcol("region", "id"),
                )),
            },
        ],
        group_by: vec![qcol("region", "id")],
        order_by: vec![SortKey {
            expr: col("region_id"),
            dir: Dir::Asc,
        }],
        select: Some(vec![
            Projection::Aliased {
                name: "region_id".into(),
                expr: qcol("region", "id"),
            },
            Projection::Aliased {
                name: "headcount".into(),
                expr: Expr::Agg {
                    func: AggFunc::Count,
                    arg: Box::new(qcol("emp", "id")),
                },
            },
        ]),
        ..ClauseSelect::default()
    }))
}

#[test]
fn inner_join_group_matches_across_both_surfaces() {
    let db = Database::create_memory().unwrap();
    for (name, cols) in [
        ("region", vec!["id"]),
        ("dept", vec!["id", "region_id"]),
        ("emp", vec!["id", "dept_id"]),
    ] {
        db.create_table(TableDef::new(
            name,
            cols.iter()
                .map(|c| ColumnDef::new(*c, TypeKind::I64).not_null())
                .collect(),
            vec!["id"],
        ))
        .unwrap();
    }
    // 2 regions, 3 depts, 6 employees.
    for id in 1..=2 {
        insert(&db, "region", vec![("id", id)]);
    }
    for (id, region) in [(1, 1), (2, 1), (3, 2)] {
        insert(&db, "dept", vec![("id", id), ("region_id", region)]);
    }
    for (id, dept) in [(1, 1), (2, 1), (3, 2), (4, 2), (5, 3), (6, 3)] {
        insert(&db, "emp", vec![("id", id), ("dept_id", dept)]);
    }

    let read = |sel: Select| -> Vec<(i64, i64)> {
        db.execute(&Request::Select(sel))
            .unwrap()
            .rows()
            .map(|r| {
                (
                    r.get_i64("region_id").unwrap().unwrap(),
                    r.get_i64("headcount").unwrap().unwrap(),
                )
            })
            .collect()
    };

    let pipeline = read(three_table_pipeline());
    let clause = read(three_table_clause());
    // region 1 has depts 1,2 → emps 1,2,3,4 (4); region 2 has dept 3 → emps 5,6 (2).
    assert_eq!(pipeline, vec![(1, 4), (2, 2)]);
    assert_eq!(
        pipeline, clause,
        "the pipeline and clause surfaces must agree"
    );
}

// --- the bank fixture (scenarios 5, 6, 7) ------------------------------------

/// accounts(id pk, balance guarded not-null CHECK(balance >= 0), version rowversion),
/// seeded with one account of balance 100.
fn bank() -> Database<common::MemoryBackend> {
    let db = Database::create_memory().unwrap();
    db.create_table(
        TableDef::new(
            "accounts",
            vec![
                ColumnDef::new("id", TypeKind::I64),
                ColumnDef::new("balance", TypeKind::I64)
                    .not_null()
                    .guarded(),
                ColumnDef::new("version", TypeKind::I64).rowversion(),
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
    insert(&db, "accounts", vec![("id", 1), ("balance", 100)]);
    db
}

/// `balance = balance - x WHERE id = 1 AND balance >= x` — a guarded relative
/// withdrawal that can never overdraw.
fn withdraw(x: i64) -> Request {
    Request::Update(Update {
        table: "accounts".into(),
        selector: Some(Selector::Where(Expr::And(vec![
            cmp(CmpOp::Eq, col("id"), lit(1)),
            cmp(CmpOp::Gte, col("balance"), lit(x)),
        ]))),
        set: vec![(
            "balance".into(),
            Expr::Arith {
                op: ArithOp::Sub,
                lhs: Box::new(col("balance")),
                rhs: Box::new(lit(x)),
            },
        )],
        unconditional: false,
    })
}

fn balance_of(db: &Database<common::MemoryBackend>) -> i64 {
    let out = db
        .execute(&Request::Select(Select::Pipeline(vec![Stage::Scan(tref(
            "accounts",
        ))])))
        .unwrap();
    out.row(0).unwrap().get_i64("balance").unwrap().unwrap()
}

// --- scenario 5: the bank scenario (headline concurrency) --------------------

#[test]
fn bank_two_concurrent_withdrawals_serialize() {
    // Two withdrawals {70, 50} from 100 sum to 120, so exactly one can succeed.
    // The writer serializes them; the loser fails its guard and changes nothing.
    // Run many times with a barrier to maximize contention.
    for _ in 0..200 {
        let db = bank();
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for x in [70, 50] {
            let db = db.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let out = db.execute(&withdraw(x)).unwrap();
                (x, out.affected().unwrap())
            }));
        }
        let outcomes: Vec<(i64, u64)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let winners: Vec<i64> = outcomes
            .iter()
            .filter(|(_, affected)| *affected == 1)
            .map(|(x, _)| *x)
            .collect();
        assert_eq!(winners.len(), 1, "exactly one withdrawal must succeed");
        assert_eq!(balance_of(&db), 100 - winners[0]);
        assert!(balance_of(&db) >= 0, "the CHECK (balance >= 0) must hold");
        db.check().unwrap();
    }
}

// --- scenario 6: optimistic (version-guarded) conflict -----------------------

#[test]
fn optimistic_version_guard_is_first_committer_wins() {
    let db = bank();
    // Both clients read version 1 and attempt a version-guarded absolute set.
    let at_v1 = |new_balance: i64| {
        Request::Update(Update {
            table: "accounts".into(),
            selector: Some(Selector::Where(Expr::And(vec![
                cmp(CmpOp::Eq, col("id"), lit(1)),
                cmp(CmpOp::Eq, col("version"), lit(1)),
            ]))),
            set: vec![("balance".into(), lit(new_balance))],
            unconditional: false,
        })
    };

    // The first commit wins and bumps the row version to 2.
    let a = db.execute(&at_v1(80)).unwrap();
    assert_eq!(a.affected(), Some(1));
    // The second still guards on version 1, now stale → it matches nothing.
    let b = db.execute(&at_v1(60)).unwrap();
    assert_eq!(b.affected(), Some(0));
    assert_eq!(balance_of(&db), 80);
}

// --- scenario 7: guard-rule enforcement --------------------------------------

#[test]
fn a_blind_set_to_a_guarded_column_is_rejected() {
    let db = bank();
    // A guarded column may only be written under a selector that reads it; a
    // blind absolute set (`unconditional`) is a validation error.
    let err = db
        .execute(&Request::Update(Update {
            table: "accounts".into(),
            selector: Some(Selector::All),
            set: vec![("balance".into(), lit(0))],
            unconditional: true,
        }))
        .unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Validation);
    assert_eq!(balance_of(&db), 100, "the rejected write changed nothing");
}

#[test]
fn a_selectorless_update_or_delete_is_rejected() {
    let db = bank();
    // No `where` and no `{all:true}` → rejected (§6 rule 1) for both update…
    let update = db
        .execute(&Request::Update(Update {
            table: "accounts".into(),
            selector: None,
            set: vec![("balance".into(), lit(50))],
            unconditional: false,
        }))
        .unwrap_err();
    assert_eq!(update.category(), ErrorCategory::Validation);

    // …and delete.
    let delete = db
        .execute(&Request::Delete(otf_dbms::Delete {
            table: "accounts".into(),
            selector: None,
        }))
        .unwrap_err();
    assert_eq!(delete.category(), ErrorCategory::Validation);
    assert_eq!(balance_of(&db), 100);
}

// --- scenario 8: crash durability --------------------------------------------

type FaultBackend = Arc<FaultInjectingBackend<MemoryBackend>>;

fn services() -> (Arc<dyn Clock>, Arc<dyn Rng>) {
    (Arc::new(SystemClock), Arc::new(SeededRng::new(0x0C0FFEE)))
}

/// A fault backend with 20 committed `items` rows as the durable baseline.
fn crash_baseline() -> FaultBackend {
    let backend: FaultBackend = Arc::new(FaultInjectingBackend::new(MemoryBackend::new()));
    let (clock, rng) = services();
    let db = Database::create_with(Arc::clone(&backend), clock, rng).unwrap();
    db.create_table(TableDef::new(
        "items",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("v", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    ))
    .unwrap();
    for id in 0..20 {
        insert(&db, "items", vec![("id", id), ("v", id * 10)]);
    }
    db.close();
    backend
}

fn scan_items() -> Request {
    Request::Select(Select::Pipeline(vec![Stage::Scan(tref("items"))]))
}

#[test]
fn a_crash_during_a_write_recovers_with_no_torn_state() {
    // A fresh-page commit writes data pages, syncs, writes the meta, and syncs
    // again. Trip each I/O occurrence in turn: whatever the file holds at the
    // crash, a reopen is valid (integrity check passes), keeps every committed
    // row, and the interrupted insert is atomic — fully present or fully absent,
    // never torn or phantom.
    for point in [FaultPoint::Sync, FaultPoint::Write] {
        for occurrence in 1u64..=6 {
            let backend = crash_baseline();
            backend.reset_counters();
            backend.arm(point, occurrence);

            let (clock, rng) = services();
            let db = Database::open_with(Arc::clone(&backend), clock, rng).unwrap();
            let result = db.execute(&Request::Insert(Insert {
                table: "items".into(),
                rows: vec![vec![
                    ("id".into(), Value::I64(999)),
                    ("v".into(), Value::I64(999)),
                ]],
            }));
            db.close();
            backend.disarm();

            // Reopen the crashed file: it must recover cleanly.
            let (clock, rng) = services();
            let db = Database::open_with(Arc::clone(&backend), clock, rng).unwrap();
            db.check()
                .unwrap_or_else(|e| panic!("{point:?}#{occurrence}: recovered file corrupt: {e}"));

            let out = db.execute(&scan_items()).unwrap();
            let ids: Vec<i64> = out
                .rows()
                .map(|r| r.get_i64("id").unwrap().unwrap())
                .collect();
            for id in 0..20 {
                assert!(
                    ids.contains(&id),
                    "{point:?}#{occurrence}: committed baseline row {id} lost"
                );
            }
            let has_extra = ids.contains(&999);
            if result.is_ok() {
                assert!(
                    has_extra,
                    "{point:?}#{occurrence}: an acknowledged insert was lost on recovery"
                );
            }
            assert_eq!(
                out.len(),
                20 + usize::from(has_extra),
                "{point:?}#{occurrence}: row-count drift after recovery"
            );
            db.close();
        }
    }
}
