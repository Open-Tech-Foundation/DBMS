//! Phase 9 reference executor over a live database: build a schema, insert
//! rows, then lower → validate → execute and assert the materialized result.
//! The reference executor is the correctness oracle for the pull-based
//! executor; here it is exercised directly as the first end-to-end read path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use catalog::{Catalog, ColumnDef, TableDef};
use common::{ManualClock, MemoryBackend, SeededRng};
use proto::{
    AggFunc, ArithOp, ClauseSelect, CmpOp, Dir, Expr, JoinKind, JoinSpec, Plan, Projection, Select,
    SortKey, Stage, TableRef,
};
use query::{execute_reference, lower, validate_select, Relation};
use types::{TypeKind, Value};

// --- fixtures ----------------------------------------------------------------

fn db() -> Catalog<MemoryBackend> {
    let cat = Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_000_000)),
        Arc::new(SeededRng::new(1)),
    )
    .unwrap();
    cat.create_table(TableDef::new(
        "users",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("name", TypeKind::Text).not_null(),
            ColumnDef::new("city", TypeKind::Text),
        ],
        vec!["id"],
    ))
    .unwrap();
    cat.create_table(TableDef::new(
        "orders",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("user_id", TypeKind::I64).not_null(),
            ColumnDef::new("amount", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    ))
    .unwrap();

    for (id, name, city) in [
        (1, "ada", "paris"),
        (2, "bob", "paris"),
        (3, "cyd", "berlin"),
    ] {
        cat.insert(
            "users",
            vec![
                ("id".into(), Value::I64(id)),
                ("name".into(), Value::Text(name.into())),
                ("city".into(), Value::Text(city.into())),
            ],
        )
        .unwrap();
    }
    for (id, user, amount) in [(1, 1, 100), (2, 1, 50), (3, 2, 70), (4, 3, 30)] {
        cat.insert(
            "orders",
            vec![
                ("id".into(), Value::I64(id)),
                ("user_id".into(), Value::I64(user)),
                ("amount".into(), Value::I64(amount)),
            ],
        )
        .unwrap();
    }
    cat
}

fn run(cat: &Catalog<MemoryBackend>, select: &Select) -> Relation {
    let snap = cat.snapshot();
    // Every query is validated before it executes.
    validate_select(select, &snap).unwrap();
    let plan = lower(select).unwrap();
    execute_reference(&plan, &snap).unwrap()
}

fn col(name: &str) -> Expr {
    Expr::Column {
        table: None,
        column: name.to_string(),
    }
}

fn qcol(t: &str, name: &str) -> Expr {
    Expr::Column {
        table: Some(t.to_string()),
        column: name.to_string(),
    }
}

fn lit(n: i64) -> Expr {
    Expr::Literal(Value::I64(n))
}

fn cmp(op: CmpOp, l: Expr, r: Expr) -> Expr {
    Expr::Cmp {
        op,
        lhs: Box::new(l),
        rhs: Box::new(r),
    }
}

fn tref(table: &str, alias: Option<&str>) -> TableRef {
    TableRef {
        table: table.into(),
        alias: alias.map(str::to_string),
    }
}

// --- tests -------------------------------------------------------------------

#[test]
fn scan_returns_all_rows_in_pk_order() {
    let rel = run(
        &db(),
        &Select::Pipeline(vec![Stage::Scan(tref("users", None))]),
    );
    assert_eq!(rel.rows.len(), 3);
    assert_eq!(rel.rows[0][1], Value::Text("ada".into()));
    assert_eq!(rel.rows[2][2], Value::Text("berlin".into()));
}

#[test]
fn filter_keeps_only_matching_rows() {
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Match(cmp(CmpOp::Gte, col("amount"), lit(70))),
    ]);
    let rel = run(&db(), &sel);
    let amounts: Vec<&Value> = rel.rows.iter().map(|r| &r[2]).collect();
    assert_eq!(amounts, [&Value::I64(100), &Value::I64(70)]);
}

#[test]
fn projection_and_computed_column() {
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Project(vec![
            Projection::Expr(col("id")),
            Projection::Aliased {
                name: "with_tax".into(),
                expr: Expr::Arith {
                    op: ArithOp::Add,
                    lhs: Box::new(col("amount")),
                    rhs: Box::new(lit(5)),
                },
            },
        ]),
    ]);
    let rel = run(&db(), &sel);
    assert_eq!(rel.shape.cols[1].name, "with_tax");
    assert_eq!(rel.rows[0], vec![Value::I64(1), Value::I64(105)]);
}

#[test]
fn inner_join_matches_pairs() {
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("users", Some("u"))),
        Stage::Join(JoinSpec {
            kind: JoinKind::Inner,
            table: tref("orders", Some("o")),
            on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
        }),
        Stage::Project(vec![
            Projection::Expr(qcol("u", "name")),
            Projection::Expr(qcol("o", "amount")),
        ]),
    ]);
    let rel = run(&db(), &sel);
    // ada has 2 orders, bob 1, cyd 1 → 4 rows.
    assert_eq!(rel.rows.len(), 4);
    assert!(rel
        .rows
        .contains(&vec![Value::Text("ada".into()), Value::I64(100)]));
    assert!(rel
        .rows
        .contains(&vec![Value::Text("cyd".into()), Value::I64(30)]));
}

#[test]
fn left_join_keeps_unmatched_left_rows() {
    // Give one user no orders by filtering orders to a user that exists but
    // has none in scope — simpler: left-join users↔orders where user has no
    // order (none here), so add a user via a fresh db.
    let cat = db();
    cat.insert(
        "users",
        vec![
            ("id".into(), Value::I64(4)),
            ("name".into(), Value::Text("dee".into())),
            ("city".into(), Value::Text("rome".into())),
        ],
    )
    .unwrap();

    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("users", Some("u"))),
        Stage::Join(JoinSpec {
            kind: JoinKind::Left,
            table: tref("orders", Some("o")),
            on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
        }),
        Stage::Match(cmp(
            CmpOp::Eq,
            qcol("u", "name"),
            Expr::Literal(Value::Text("dee".into())),
        )),
    ]);
    let rel = run(&cat, &sel);
    // dee has no orders → one row with null order columns.
    assert_eq!(rel.rows.len(), 1);
    let row = &rel.rows[0];
    // u.id, u.name, u.city, o.id, o.user_id, o.amount
    assert_eq!(row[1], Value::Text("dee".into()));
    assert_eq!(row[3], Value::Null);
    assert_eq!(row[5], Value::Null);
}

#[test]
fn cross_join_is_the_full_product() {
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("users", None)),
        Stage::Join(JoinSpec {
            kind: JoinKind::Cross,
            table: tref("orders", None),
            on: None,
        }),
    ]);
    let rel = run(&db(), &sel);
    assert_eq!(rel.rows.len(), 3 * 4);
}

#[test]
fn group_by_with_aggregates() {
    // Per city: number of users and (via join) total spend — but keep it to
    // one table here: group orders by user_id, sum + count + min + max + avg.
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Group {
            by: vec![col("user_id")],
            aggs: vec![
                (
                    "spent".into(),
                    Expr::Agg {
                        func: AggFunc::Sum,
                        arg: Box::new(col("amount")),
                    },
                ),
                (
                    "n".into(),
                    Expr::Agg {
                        func: AggFunc::Count,
                        arg: Box::new(lit(1)),
                    },
                ),
                (
                    "smallest".into(),
                    Expr::Agg {
                        func: AggFunc::Min,
                        arg: Box::new(col("amount")),
                    },
                ),
            ],
        },
        Stage::Project(vec![
            Projection::Expr(col("user_id")),
            Projection::Expr(col("spent")),
            Projection::Expr(col("n")),
            Projection::Expr(col("smallest")),
        ]),
    ]);
    let rel = run(&db(), &sel);
    // user 1: 100+50=150 (2 orders, min 50); user 2: 70 (1); user 3: 30 (1).
    assert!(rel.rows.contains(&vec![
        Value::I64(1),
        Value::I64(150),
        Value::I64(2),
        Value::I64(50)
    ]));
    assert!(rel.rows.contains(&vec![
        Value::I64(2),
        Value::I64(70),
        Value::I64(1),
        Value::I64(70)
    ]));
}

#[test]
fn having_filters_groups() {
    // HAVING is a match after group: keep users who spent > 100.
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Group {
            by: vec![col("user_id")],
            aggs: vec![(
                "spent".into(),
                Expr::Agg {
                    func: AggFunc::Sum,
                    arg: Box::new(col("amount")),
                },
            )],
        },
        Stage::Match(cmp(CmpOp::Gt, col("spent"), lit(100))),
        Stage::Project(vec![Projection::Expr(col("user_id"))]),
    ]);
    let rel = run(&db(), &sel);
    assert_eq!(rel.rows, vec![vec![Value::I64(1)]]); // only user 1 (150)
}

#[test]
fn global_aggregate_over_all_rows() {
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Group {
            by: vec![],
            aggs: vec![
                (
                    "total".into(),
                    Expr::Agg {
                        func: AggFunc::Sum,
                        arg: Box::new(col("amount")),
                    },
                ),
                (
                    "avg_amount".into(),
                    Expr::Agg {
                        func: AggFunc::Avg,
                        arg: Box::new(col("amount")),
                    },
                ),
            ],
        },
        Stage::Project(vec![
            Projection::Expr(col("total")),
            Projection::Expr(col("avg_amount")),
        ]),
    ]);
    let rel = run(&db(), &sel);
    assert_eq!(rel.rows.len(), 1);
    assert_eq!(rel.rows[0][0], Value::I64(250)); // 100+50+70+30
    assert_eq!(rel.rows[0][1], Value::F64(62.5));
}

#[test]
fn sort_distinct_limit_offset() {
    // Distinct cities, sorted descending, skip 1.
    let sel = Select::Pipeline(vec![
        Stage::Scan(tref("users", None)),
        Stage::Project(vec![Projection::Expr(col("city"))]),
        Stage::Distinct(true),
        Stage::Sort(vec![SortKey {
            expr: col("city"),
            dir: Dir::Desc,
        }]),
        Stage::Limit {
            limit: Some(1),
            offset: 1,
        },
    ]);
    let rel = run(&db(), &sel);
    // cities distinct = {paris, berlin}; desc = [paris, berlin]; skip 1 → [berlin].
    assert_eq!(rel.rows, vec![vec![Value::Text("berlin".into())]]);
}

#[test]
fn pipeline_and_clause_forms_execute_identically() {
    // Join + group + having, written in both surfaces (acceptance scenario 3).
    let pipeline = Select::Pipeline(vec![
        Stage::Scan(tref("users", Some("u"))),
        Stage::Join(JoinSpec {
            kind: JoinKind::Inner,
            table: tref("orders", Some("o")),
            on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
        }),
        Stage::Group {
            by: vec![qcol("u", "city")],
            aggs: vec![(
                "spent".into(),
                Expr::Agg {
                    func: AggFunc::Sum,
                    arg: Box::new(qcol("o", "amount")),
                },
            )],
        },
        Stage::Project(vec![
            Projection::Expr(qcol("u", "city")),
            Projection::Expr(col("spent")),
        ]),
        Stage::Sort(vec![SortKey {
            expr: col("city"),
            dir: Dir::Asc,
        }]),
    ]);
    let clause = Select::Clause(Box::new(ClauseSelect {
        from: Some(tref("users", Some("u"))),
        joins: vec![JoinSpec {
            kind: JoinKind::Inner,
            table: tref("orders", Some("o")),
            on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
        }],
        group_by: vec![qcol("u", "city")],
        select: Some(vec![
            Projection::Expr(qcol("u", "city")),
            Projection::Aliased {
                name: "spent".into(),
                expr: Expr::Agg {
                    func: AggFunc::Sum,
                    arg: Box::new(qcol("o", "amount")),
                },
            },
        ]),
        order_by: vec![SortKey {
            expr: col("city"),
            dir: Dir::Asc,
        }],
        ..ClauseSelect::default()
    }));

    let cat = db();
    let a = run(&cat, &pipeline);
    let b = run(&cat, &clause);
    assert_eq!(a.rows, b.rows);
    // berlin: cyd spent 30; paris: ada 150 + bob 70 = 220.
    assert_eq!(
        a.rows,
        vec![
            vec![Value::Text("berlin".into()), Value::I64(30)],
            vec![Value::Text("paris".into()), Value::I64(220)],
        ]
    );
}

#[test]
fn index_scan_matches_a_full_scan_with_filter() {
    // An IndexScan node (planner-only) returns the same rows as Scan+Filter.
    let cat = db();
    cat.create_index("orders", catalog::IndexDef::new("by_user", vec!["user_id"]))
        .unwrap();
    let snap = cat.snapshot();

    let index_plan = Plan::IndexScan {
        table: "orders".into(),
        alias: None,
        index: "by_user".into(),
        prefix: vec![Value::I64(1)],
    };
    let scan_plan = Plan::Filter {
        input: Box::new(Plan::Scan {
            table: "orders".into(),
            alias: None,
        }),
        pred: cmp(CmpOp::Eq, col("user_id"), lit(1)),
    };
    let via_index = execute_reference(&index_plan, &snap).unwrap();
    let via_scan = execute_reference(&scan_plan, &snap).unwrap();
    assert_eq!(via_index.rows, via_scan.rows);
    assert_eq!(via_index.rows.len(), 2); // user 1 has two orders
}
