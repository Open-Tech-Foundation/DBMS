//! Phase 9 top-level engine: the full decode → validate → plan → execute →
//! encode journey for every request kind, plus atomic multi-op transactions
//! and the bytes-in/bytes-out `execute_wire` seam.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use catalog::{Catalog, ColumnDef, TableDef};
use common::{CategorizedError, ErrorCategory, ManualClock, MemoryBackend, SeededRng};
use proto::{
    decode_doc, encode_request, ArithOp, CmpOp, DecodeLimits, Delete, Doc, Expr, Insert, Request,
    Select, Selector, Stage, TableRef, Update,
};
use query::{execute_query, execute_wire};
use types::{TypeKind, Value};

fn db() -> Catalog<MemoryBackend> {
    let cat = Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_000_000)),
        Arc::new(SeededRng::new(1)),
    )
    .unwrap();
    cat.create_table(TableDef::new(
        "accounts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("balance", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    ))
    .unwrap();
    for (id, bal) in [(1, 100), (2, 50)] {
        cat.insert(
            "accounts",
            vec![
                ("id".into(), Value::I64(id)),
                ("balance".into(), Value::I64(bal)),
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

#[test]
fn select_returns_columns_and_rows() {
    let cat = db();
    let select = Select::Pipeline(vec![
        Stage::Scan(TableRef {
            table: "accounts".into(),
            alias: None,
        }),
        Stage::Match(cmp(CmpOp::Gte, col("balance"), lit(100))),
    ]);
    let result = execute_query(&Request::Select(select), &cat).unwrap();
    assert_eq!(result.columns, ["id", "balance"]);
    assert_eq!(result.rows, vec![vec![Value::I64(1), Value::I64(100)]]);
    assert_eq!(result.applied, None);
    assert_eq!(result.affected, None);
}

#[test]
fn explain_returns_a_plan_tree() {
    let cat = db();
    let select = Select::Pipeline(vec![Stage::Scan(TableRef {
        table: "accounts".into(),
        alias: None,
    })]);
    let result = execute_query(&Request::Explain(select), &cat).unwrap();
    assert_eq!(result.columns, ["plan"]);
    let text: String = result
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Scan accounts"), "unexpected plan: {text}");
}

#[test]
fn update_reports_applied_and_affected() {
    let cat = db();
    let update = Update {
        table: "accounts".into(),
        selector: Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
        set: vec![(
            "balance".into(),
            Expr::Arith {
                op: ArithOp::Add,
                lhs: Box::new(col("balance")),
                rhs: Box::new(lit(10)),
            },
        )],
        unconditional: false,
    };
    let result = execute_query(&Request::Update(update), &cat).unwrap();
    assert_eq!(result.applied, Some(true));
    assert_eq!(result.affected, Some(1));
    let row = cat
        .snapshot()
        .get("accounts", &[Value::I64(1)])
        .unwrap()
        .unwrap();
    assert_eq!(row[1], Value::I64(110));
}

#[test]
fn delete_reports_affected() {
    let cat = db();
    let delete = Delete {
        table: "accounts".into(),
        selector: Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(2)))),
    };
    let result = execute_query(&Request::Delete(delete), &cat).unwrap();
    assert_eq!(result.affected, Some(1));
    assert_eq!(cat.snapshot().scan("accounts").unwrap().len(), 1);
}

#[test]
fn transaction_commits_all_ops_atomically() {
    let cat = db();
    let txn = Request::Transaction(vec![
        Request::Insert(Insert {
            table: "accounts".into(),
            rows: vec![vec![
                ("id".into(), Value::I64(3)),
                ("balance".into(), Value::I64(5)),
            ]],
        }),
        Request::Update(Update {
            table: "accounts".into(),
            selector: Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
            set: vec![("balance".into(), lit(200))],
            unconditional: true,
        }),
    ]);
    let result = execute_query(&txn, &cat).unwrap();
    assert_eq!(result.affected, Some(2)); // 1 inserted + 1 updated
    let snap = cat.snapshot();
    assert_eq!(
        snap.get("accounts", &[Value::I64(3)]).unwrap().unwrap()[1],
        Value::I64(5)
    );
    assert_eq!(
        snap.get("accounts", &[Value::I64(1)]).unwrap().unwrap()[1],
        Value::I64(200)
    );
}

#[test]
fn transaction_rolls_back_on_a_failing_op() {
    let cat = db();
    // The second insert duplicates the first's primary key: the whole
    // transaction must fail and commit nothing.
    let txn = Request::Transaction(vec![
        Request::Insert(Insert {
            table: "accounts".into(),
            rows: vec![vec![
                ("id".into(), Value::I64(9)),
                ("balance".into(), Value::I64(1)),
            ]],
        }),
        Request::Insert(Insert {
            table: "accounts".into(),
            rows: vec![vec![
                ("id".into(), Value::I64(9)),
                ("balance".into(), Value::I64(2)),
            ]],
        }),
    ]);
    let err = execute_query(&txn, &cat).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Constraint);
    // Nothing committed — id 9 is absent.
    assert!(cat
        .snapshot()
        .get("accounts", &[Value::I64(9)])
        .unwrap()
        .is_none());
}

#[test]
fn execute_wire_produces_a_well_formed_result() {
    let cat = db();
    let select = Select::Pipeline(vec![Stage::Scan(TableRef {
        table: "accounts".into(),
        alias: None,
    })]);
    let wire = encode_request(&Request::Select(select));
    let response = execute_wire(&wire, &cat);

    // The response decodes as a well-formed result map with ok:true and 2 rows.
    let doc = decode_doc(&response, &DecodeLimits::default()).unwrap();
    let Doc::Map(entries) = doc else {
        panic!("result is not a map");
    };
    let field = |key: &str| entries.iter().find(|(k, _)| k == key).map(|(_, v)| v);
    assert_eq!(field("ok"), Some(&Doc::Bool(true)));
    match field("rows") {
        Some(Doc::Array(rows)) => assert_eq!(rows.len(), 2),
        other => panic!("unexpected rows field: {other:?}"),
    }
}

#[test]
fn execute_wire_reports_errors_as_a_result() {
    let cat = db();
    // A select over a missing table → a typed error result, not a panic.
    let select = Select::Pipeline(vec![Stage::Scan(TableRef {
        table: "ghosts".into(),
        alias: None,
    })]);
    let wire = encode_request(&Request::Select(select));
    let response = execute_wire(&wire, &cat);
    let doc = decode_doc(&response, &DecodeLimits::default()).unwrap();
    let Doc::Map(entries) = doc else {
        panic!("result is not a map");
    };
    let field = |key: &str| entries.iter().find(|(k, _)| k == key).map(|(_, v)| v);
    assert_eq!(field("ok"), Some(&Doc::Bool(false)));
    assert_eq!(field("code"), Some(&Doc::Str("validation".to_string())));
}
