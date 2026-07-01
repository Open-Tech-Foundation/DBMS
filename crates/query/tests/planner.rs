//! Phase 9 planner + EXPLAIN: index selection (acceptance scenario 2), filter
//! pushdown, and the invariant that planning is semantics-preserving — the
//! **planned** plan executes to exactly the reference executor's rows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use catalog::{Catalog, ColumnDef, IndexDef, TableDef};
use common::{ManualClock, MemoryBackend, SeededRng};
use proto::{CmpOp, Expr, JoinKind, JoinSpec, Plan, Select, Stage, TableRef};
use query::{execute_reference, explain, lower, plan};
use types::{TypeKind, Value};

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
    cat.create_index("orders", IndexDef::new("by_user", vec!["user_id"]))
        .unwrap();

    for (id, city) in [(1, "paris"), (2, "berlin")] {
        cat.insert(
            "users",
            vec![
                ("id".into(), Value::I64(id)),
                ("city".into(), Value::Text(city.into())),
            ],
        )
        .unwrap();
    }
    for (id, user, amount) in [(1, 1, 100), (2, 1, 50), (3, 2, 70)] {
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

/// Assert that the logical and planned plans execute to identical rows.
fn assert_plan_preserves(cat: &Catalog<MemoryBackend>, select: &Select) -> Plan {
    let snap = cat.snapshot();
    let logical = lower(select).unwrap();
    let physical = plan(&logical, &snap).unwrap();
    let a = execute_reference(&logical, &snap).unwrap();
    let b = execute_reference(&physical, &snap).unwrap();
    assert_eq!(a.rows, b.rows, "planning changed the result rows");
    physical
}

#[test]
fn equality_on_indexed_column_becomes_an_index_scan() {
    // orders WHERE user_id = 1  →  IndexScan orders using by_user prefix=[1].
    let select = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Match(cmp(CmpOp::Eq, col("user_id"), lit(1))),
    ]);
    let physical = assert_plan_preserves(&db(), &select);
    match physical {
        Plan::IndexScan { index, prefix, .. } => {
            assert_eq!(index, "by_user");
            assert_eq!(prefix, vec![Value::I64(1)]);
        }
        other => panic!("expected an IndexScan, got {other:?}"),
    }
}

#[test]
fn index_scan_keeps_a_residual_filter() {
    // orders WHERE user_id = 1 AND amount > 50  →  IndexScan + Filter(amount>50).
    let select = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Match(Expr::And(vec![
            cmp(CmpOp::Eq, col("user_id"), lit(1)),
            cmp(CmpOp::Gt, col("amount"), lit(50)),
        ])),
    ]);
    let physical = assert_plan_preserves(&db(), &select);
    match physical {
        Plan::Filter { input, .. } => {
            assert!(matches!(*input, Plan::IndexScan { .. }));
        }
        other => panic!("expected Filter over IndexScan, got {other:?}"),
    }
}

#[test]
fn explain_reports_index_usage() {
    // Acceptance scenario 2: EXPLAIN shows the seek uses the index.
    let cat = db();
    let snap = cat.snapshot();
    let select = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Match(cmp(CmpOp::Eq, col("user_id"), lit(1))),
    ]);
    let text = explain(&select, &snap).unwrap();
    assert!(
        text.contains("IndexScan"),
        "EXPLAIN missing IndexScan:\n{text}"
    );
    assert!(
        text.contains("by_user"),
        "EXPLAIN missing index name:\n{text}"
    );
}

#[test]
fn filter_over_join_is_pushed_to_the_matching_side() {
    // Join users↔orders, then filter on a users-only column: the filter is
    // pushed onto the users side, and a filter on the indexed orders column is
    // pushed onto orders (becoming an index scan).
    let select = Select::Pipeline(vec![
        Stage::Scan(tref("users", Some("u"))),
        Stage::Join(JoinSpec {
            kind: JoinKind::Inner,
            table: tref("orders", Some("o")),
            on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
        }),
        Stage::Match(Expr::And(vec![
            cmp(
                CmpOp::Eq,
                qcol("u", "city"),
                Expr::Literal(Value::Text("paris".into())),
            ),
            cmp(CmpOp::Eq, qcol("o", "user_id"), lit(1)),
        ])),
    ]);
    let physical = assert_plan_preserves(&db(), &select);
    // The top filter should be gone: both conjuncts were single-side and pushed.
    match physical {
        Plan::Join { left, right, .. } => {
            assert!(
                matches!(*left, Plan::Filter { .. }),
                "users-side filter should be pushed down"
            );
            // The orders side used its index (user_id = 1).
            assert!(
                matches!(*right, Plan::IndexScan { .. }),
                "orders-side equality should become an index scan"
            );
        }
        other => panic!("expected a Join at the root, got {other:?}"),
    }
}

#[test]
fn left_join_filter_is_not_pushed() {
    // Pushing a right-side filter below a LEFT join would change null
    // semantics, so it must stay above the join.
    let select = Select::Pipeline(vec![
        Stage::Scan(tref("users", Some("u"))),
        Stage::Join(JoinSpec {
            kind: JoinKind::Left,
            table: tref("orders", Some("o")),
            on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
        }),
        Stage::Match(cmp(CmpOp::Eq, qcol("o", "user_id"), lit(1))),
    ]);
    let physical = assert_plan_preserves(&db(), &select);
    assert!(
        matches!(physical, Plan::Filter { .. }),
        "the filter must remain above a LEFT join"
    );
}

#[test]
fn planning_preserves_a_grouped_query() {
    let select = Select::Pipeline(vec![
        Stage::Scan(tref("orders", None)),
        Stage::Match(cmp(CmpOp::Eq, col("user_id"), lit(1))),
        Stage::Group {
            by: vec![col("user_id")],
            aggs: vec![(
                "spent".into(),
                Expr::Agg {
                    func: proto::AggFunc::Sum,
                    arg: Box::new(col("amount")),
                },
            )],
        },
    ]);
    assert_plan_preserves(&db(), &select);
}
