//! Phase 9 validator against a **live** schema source: the same validation
//! runs over a real [`catalog::CatSnapshot`] (via the `SchemaView` impl), not
//! just the in-memory test map. Proves name resolution and the `SPEC.md` §6
//! safety rules hold against committed schema.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use catalog::{Catalog, ColumnDef, TableDef};
use common::{CategorizedError, ErrorCategory, ManualClock, MemoryBackend, SeededRng};
use proto::{ArithOp, CmpOp, Delete, Expr, Insert, Request, Selector, Stage, TableRef, Update};
use query::{validate, ValidateError, Validated};
use types::{TypeKind, Value};

fn db() -> Catalog<MemoryBackend> {
    let cat = Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_000_000)),
        Arc::new(SeededRng::new(1)),
    )
    .unwrap();
    // accounts(id pk, balance i64 not-null guarded, owner text, version rowversion)
    cat.create_table(TableDef::new(
        "accounts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("balance", TypeKind::I64)
                .not_null()
                .guarded(),
            ColumnDef::new("owner", TypeKind::Text),
            ColumnDef::new("version", TypeKind::I64).rowversion(),
        ],
        vec!["id"],
    ))
    .unwrap();
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
fn select_over_live_schema_yields_output_columns() {
    let snap = db().snapshot();
    let sel = proto::Select::Pipeline(vec![Stage::Scan(TableRef {
        table: "accounts".into(),
        alias: None,
    })]);
    let Validated::Read(out) = validate(&Request::Select(sel), &snap).unwrap() else {
        panic!("expected a read");
    };
    let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "balance", "owner", "version"]);
}

#[test]
fn unknown_table_rejected_against_live_schema() {
    let snap = db().snapshot();
    let sel = proto::Select::Pipeline(vec![Stage::Scan(TableRef {
        table: "ghosts".into(),
        alias: None,
    })]);
    let err = validate(&Request::Select(sel), &snap).unwrap_err();
    assert!(matches!(err, ValidateError::UnknownTable { .. }));
    assert_eq!(err.category(), ErrorCategory::Validation);
}

#[test]
fn bank_scenario_guarded_update_validates() {
    let snap = db().snapshot();
    // balance = balance - 50 WHERE balance >= 50 — the headline guarded update.
    let update = Update {
        table: "accounts".into(),
        selector: Some(Selector::Where(cmp(CmpOp::Gte, col("balance"), lit(50)))),
        set: vec![(
            "balance".into(),
            Expr::Arith {
                op: ArithOp::Sub,
                lhs: Box::new(col("balance")),
                rhs: Box::new(lit(50)),
            },
        )],
        unconditional: false,
    };
    assert_eq!(
        validate(&Request::Update(update), &snap).unwrap(),
        Validated::Write
    );
}

#[test]
fn guarded_blind_set_rejected_against_live_schema() {
    let snap = db().snapshot();
    let update = Update {
        table: "accounts".into(),
        selector: Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
        set: vec![("balance".into(), lit(30))],
        unconditional: false,
    };
    let err = validate(&Request::Update(update), &snap).unwrap_err();
    assert!(matches!(err, ValidateError::GuardedBlindSet { .. }));
}

#[test]
fn missing_selector_rejected_against_live_schema() {
    let snap = db().snapshot();
    let delete = Delete {
        table: "accounts".into(),
        selector: None,
    };
    let err = validate(&Request::Delete(delete), &snap).unwrap_err();
    assert!(matches!(err, ValidateError::MissingSelector));
}

#[test]
fn insert_engine_managed_rejected_against_live_schema() {
    let snap = db().snapshot();
    let insert = Insert {
        table: "accounts".into(),
        rows: vec![vec![("version".into(), Value::I64(3))]],
    };
    let err = validate(&Request::Insert(insert), &snap).unwrap_err();
    assert!(matches!(err, ValidateError::EngineManagedWrite { .. }));
}
