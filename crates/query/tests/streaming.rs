//! Phase 9 pull-based executor: **result-equivalence vs the reference
//! executor** across random data/queries (the exit criterion), and **keyset
//! cursor pagination** — paging a table with continuation tokens skips and
//! duplicates no row within a snapshot (acceptance scenario 4).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use catalog::{Catalog, ColumnDef, IndexDef, TableDef};
use common::{CategorizedError, ManualClock, MemoryBackend, Rng, SeededRng};
use proto::{
    AggFunc, CmpOp, Dir, Expr, JoinKind, JoinSpec, Projection, Select, SortKey, Stage, TableRef,
};
use query::{execute_page, execute_query, execute_reference, execute_stream, lower, plan};
use types::{TypeKind, Value};

fn clock_rng() -> (Arc<ManualClock>, Arc<SeededRng>) {
    (
        Arc::new(ManualClock::new(1_000_000)),
        Arc::new(SeededRng::new(7)),
    )
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

// --- result-equivalence vs the reference executor ----------------------------

#[test]
fn streaming_matches_reference_over_random_data() {
    let (clock, _) = clock_rng();
    let rng = SeededRng::new(42);
    let cat = Catalog::create(MemoryBackend::new(), clock, Arc::new(SeededRng::new(9))).unwrap();
    cat.create_table(TableDef::new(
        "users",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("dept", TypeKind::I64).not_null(),
            ColumnDef::new("age", TypeKind::I64).not_null(),
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

    // Random rows.
    for id in 1..=40 {
        let dept = (rng.next_u64() % 4) as i64;
        let age = (rng.next_u64() % 60) as i64;
        cat.insert(
            "users",
            vec![
                ("id".into(), Value::I64(id)),
                ("dept".into(), Value::I64(dept)),
                ("age".into(), Value::I64(age)),
            ],
        )
        .unwrap();
    }
    for id in 1..=60 {
        let user = (rng.next_u64() % 40 + 1) as i64;
        let amount = (rng.next_u64() % 500) as i64;
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
    let snap = cat.snapshot();

    // A spread of query shapes: filter, join, group, sort, distinct, project.
    let queries: Vec<Select> = vec![
        Select::Pipeline(vec![
            Stage::Scan(tref("users", None)),
            Stage::Match(cmp(CmpOp::Gte, col("age"), lit(30))),
        ]),
        Select::Pipeline(vec![
            Stage::Scan(tref("users", Some("u"))),
            Stage::Join(JoinSpec {
                kind: JoinKind::Inner,
                table: tref("orders", Some("o")),
                on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
            }),
            Stage::Match(cmp(CmpOp::Gt, qcol("o", "amount"), lit(200))),
            Stage::Project(vec![
                Projection::Expr(qcol("u", "id")),
                Projection::Expr(qcol("o", "amount")),
            ]),
        ]),
        Select::Pipeline(vec![
            Stage::Scan(tref("users", Some("u"))),
            Stage::Join(JoinSpec {
                kind: JoinKind::Left,
                table: tref("orders", Some("o")),
                on: Some(cmp(CmpOp::Eq, qcol("u", "id"), qcol("o", "user_id"))),
            }),
        ]),
        Select::Pipeline(vec![
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
                ],
            },
            Stage::Match(cmp(CmpOp::Gt, col("spent"), lit(300))),
        ]),
        Select::Pipeline(vec![
            Stage::Scan(tref("users", None)),
            Stage::Project(vec![Projection::Expr(col("dept"))]),
            Stage::Distinct(true),
            Stage::Sort(vec![SortKey {
                expr: col("dept"),
                dir: Dir::Desc,
            }]),
        ]),
    ];

    for select in &queries {
        let physical = plan(&lower(select).unwrap(), &snap).unwrap();
        let reference = execute_reference(&physical, &snap).unwrap();
        let streamed = execute_stream(&physical, &snap).unwrap();
        assert_eq!(
            reference.rows, streamed.rows,
            "streaming and reference disagree on a query"
        );
    }
}

// --- keyset cursor pagination ------------------------------------------------

fn paging_db() -> Catalog<MemoryBackend> {
    let (clock, rng) = clock_rng();
    let cat = Catalog::create(MemoryBackend::new(), clock, rng).unwrap();
    cat.create_table(TableDef::new(
        "items",
        vec![ColumnDef::new("id", TypeKind::I64)],
        vec!["id"],
    ))
    .unwrap();
    cat.create_index("items", IndexDef::new("by_id", vec!["id"]))
        .unwrap();
    for id in 1..=25 {
        cat.insert("items", vec![("id".into(), Value::I64(id))])
            .unwrap();
    }
    cat
}

/// A page-5-ordered-by-id query, resuming from `cursor`.
fn page_query(cursor: Option<Vec<u8>>) -> Select {
    let mut stages = vec![
        Stage::Scan(tref("items", None)),
        Stage::Sort(vec![SortKey {
            expr: col("id"),
            dir: Dir::Asc,
        }]),
    ];
    if let Some(token) = cursor {
        stages.push(Stage::Cursor(token));
    }
    stages.push(Stage::Limit {
        limit: Some(5),
        offset: 0,
    });
    Select::Pipeline(stages)
}

#[test]
fn keyset_pagination_walks_every_row_once() {
    let cat = paging_db();
    let mut seen: Vec<i64> = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    let mut pages = 0;

    loop {
        let result =
            execute_query(&proto::Request::Select(page_query(cursor.clone())), &cat).unwrap();
        for row in &result.rows {
            match row[0] {
                Value::I64(n) => seen.push(n),
                _ => panic!("id is not an int"),
            }
        }
        pages += 1;
        assert!(pages <= 10, "pagination did not terminate");
        match result.cursor {
            Some(token) => cursor = Some(token),
            None => break,
        }
    }

    // 25 rows over pages of 5 → 5 pages, every id exactly once, in order.
    assert_eq!(pages, 5);
    assert_eq!(seen, (1..=25).collect::<Vec<_>>());
}

#[test]
fn last_page_has_no_cursor() {
    let cat = paging_db();
    // Page size 25 returns everything at once → no continuation.
    let select = {
        let mut stages = vec![
            Stage::Scan(tref("items", None)),
            Stage::Sort(vec![SortKey {
                expr: col("id"),
                dir: Dir::Asc,
            }]),
        ];
        stages.push(Stage::Limit {
            limit: Some(25),
            offset: 0,
        });
        Select::Pipeline(stages)
    };
    let result = execute_query(&proto::Request::Select(select), &cat).unwrap();
    assert_eq!(result.rows.len(), 25);
    assert!(result.cursor.is_none());
}

#[test]
fn a_tampered_cursor_token_is_rejected() {
    let cat = paging_db();
    // Grab a real token from page 1, then corrupt it.
    let first = execute_query(&proto::Request::Select(page_query(None)), &cat).unwrap();
    let mut token = first.cursor.unwrap();
    *token.last_mut().unwrap() ^= 0xFF; // flip bits in the payload

    let err = execute_query(&proto::Request::Select(page_query(Some(token))), &cat).unwrap_err();
    assert_eq!(
        err.category(),
        common::ErrorCategory::Validation,
        "a mangled cursor must be a clean validation error"
    );
}

// Bring the page executor into scope for a direct-call smoke test.
#[test]
fn execute_page_reports_a_cursor_on_a_full_page() {
    let cat = paging_db();
    let physical = plan(&lower(&page_query(None)).unwrap(), &cat.snapshot()).unwrap();
    let page = execute_page(&physical, &cat.snapshot()).unwrap();
    assert_eq!(page.rows.len(), 5);
    assert!(
        page.cursor.is_some(),
        "a full first page should carry a cursor"
    );
}
