//! Phase 9 write path: guarded relative updates, the optimistic (version)
//! path, conditional deletes, and the headline **bank scenario** under
//! concurrency (`SPEC.md` §6, acceptance scenarios 5–7).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Barrier};
use std::thread;

use catalog::{Catalog, CheckExpr, ColumnDef, TableDef};
use common::{CategorizedError, ErrorCategory, ManualClock, MemoryBackend, SeededRng};
use proto::{ArithOp, CmpOp, Delete, Expr, Request, Selector, Update};
use query::{execute_write, validate, WriteOutcome};
use types::{TypeKind, Value};

// --- fixtures ----------------------------------------------------------------

/// accounts(id pk, balance i64 not-null guarded CHECK(balance >= 0), version rowversion).
fn bank() -> Catalog<MemoryBackend> {
    let cat = Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_000_000)),
        Arc::new(SeededRng::new(1)),
    )
    .unwrap();
    cat.create_table(
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
            op: catalog::CmpOp::Gte,
            value: Value::I64(0),
        }),
    )
    .unwrap();
    cat.insert(
        "accounts",
        vec![
            ("id".into(), Value::I64(1)),
            ("balance".into(), Value::I64(100)),
        ],
    )
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

/// A guarded relative withdrawal: `balance = balance - x WHERE id=1 AND balance >= x`.
fn withdraw(x: i64) -> Update {
    Update {
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
    }
}

fn balance(cat: &Catalog<MemoryBackend>) -> i64 {
    let row = cat
        .snapshot()
        .get("accounts", &[Value::I64(1)])
        .unwrap()
        .unwrap();
    match row[1] {
        Value::I64(n) => n,
        _ => panic!("balance is not an int"),
    }
}

// --- tests -------------------------------------------------------------------

#[test]
fn guarded_relative_update_succeeds_and_debits() {
    let cat = bank();
    let out = execute_write(&Request::Update(withdraw(70)), &cat).unwrap();
    assert_eq!(
        out,
        WriteOutcome {
            applied: Some(true),
            affected: 1
        }
    );
    assert_eq!(balance(&cat), 30);
}

#[test]
fn guarded_update_that_fails_the_guard_applies_nothing() {
    let cat = bank();
    // Balance is 100; withdrawing 150 fails `balance >= 150` → no row matches.
    let out = execute_write(&Request::Update(withdraw(150)), &cat).unwrap();
    assert_eq!(
        out,
        WriteOutcome {
            applied: Some(false),
            affected: 0
        }
    );
    assert_eq!(balance(&cat), 100); // untouched
}

#[test]
fn bank_scenario_two_concurrent_withdrawals_serialize() {
    // The headline test (§7.5), run many times with a barrier to maximize
    // contention. The writer serializes the two guarded withdrawals of {70,
    // 50} from a balance of 100 — they sum to 120, so **exactly one** can
    // succeed. Whichever the writer runs first wins (balance 30 or 50); the
    // other fails insufficient-funds. The overdraft class is eliminated: the
    // balance is never negative and both never apply.
    for _ in 0..200 {
        let cat = bank();
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for x in [70, 50] {
            let cat = cat.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let out = execute_write(&Request::Update(withdraw(x)), &cat).unwrap();
                (x, out)
            }));
        }
        let outcomes: Vec<(i64, WriteOutcome)> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();

        let winners: Vec<i64> = outcomes
            .iter()
            .filter(|(_, o)| o.applied == Some(true))
            .map(|(x, _)| *x)
            .collect();
        assert_eq!(winners.len(), 1, "exactly one withdrawal must succeed");
        // The final balance is 100 minus exactly the winning withdrawal.
        assert_eq!(balance(&cat), 100 - winners[0]);
        assert!(balance(&cat) >= 0, "the CHECK (balance >= 0) must hold");
        // The loser failed the guard and changed nothing.
        for (_, out) in &outcomes {
            if out.applied != Some(true) {
                assert_eq!(out.affected, 0);
            }
        }
        cat.snapshot().validate().unwrap();
    }
}

#[test]
fn optimistic_version_guard_is_first_committer_wins() {
    let cat = bank();
    // Both clients read version 1 and attempt a version-guarded absolute set.
    let at_v1 = |new_balance: i64| Update {
        table: "accounts".into(),
        selector: Some(Selector::Where(Expr::And(vec![
            cmp(CmpOp::Eq, col("id"), lit(1)),
            cmp(CmpOp::Eq, col("version"), lit(1)),
        ]))),
        set: vec![("balance".into(), lit(new_balance))],
        unconditional: false,
    };
    // First commit wins.
    let a = execute_write(&Request::Update(at_v1(80)), &cat).unwrap();
    assert_eq!(a.applied, Some(true));
    // Second sees version 2 now → its `version = 1` guard fails.
    let b = execute_write(&Request::Update(at_v1(60)), &cat).unwrap();
    assert_eq!(b.applied, Some(false));
    assert_eq!(b.affected, 0);
    assert_eq!(balance(&cat), 80);
}

#[test]
fn conditional_delete_removes_matching_rows() {
    let cat = Catalog::create(
        MemoryBackend::new(),
        Arc::new(ManualClock::new(1_000_000)),
        Arc::new(SeededRng::new(2)),
    )
    .unwrap();
    cat.create_table(TableDef::new(
        "sessions",
        vec![ColumnDef::new("id", TypeKind::I64)],
        vec!["id"],
    ))
    .unwrap();
    for id in 1..=5 {
        cat.insert("sessions", vec![("id".into(), Value::I64(id))])
            .unwrap();
    }
    let delete = Delete {
        table: "sessions".into(),
        selector: Some(Selector::Where(cmp(CmpOp::Lt, col("id"), lit(3)))),
    };
    let out = execute_write(&Request::Delete(delete), &cat).unwrap();
    assert_eq!(out.affected, 2); // ids 1, 2
    assert_eq!(cat.snapshot().scan("sessions").unwrap().len(), 3);
}

#[test]
fn insert_reports_affected_rows() {
    let cat = bank();
    let insert = proto::Insert {
        table: "accounts".into(),
        rows: vec![
            vec![
                ("id".into(), Value::I64(2)),
                ("balance".into(), Value::I64(5)),
            ],
            vec![
                ("id".into(), Value::I64(3)),
                ("balance".into(), Value::I64(9)),
            ],
        ],
    };
    let out = execute_write(&Request::Insert(insert), &cat).unwrap();
    assert_eq!(out.applied, None);
    assert_eq!(out.affected, 2);
}

#[test]
fn validate_then_write_rejects_a_guarded_blind_set() {
    // The validator refuses a blind absolute set to the guarded balance before
    // it can reach the writer.
    let cat = bank();
    let update = Update {
        table: "accounts".into(),
        selector: Some(Selector::Where(cmp(CmpOp::Eq, col("id"), lit(1)))),
        set: vec![("balance".into(), lit(30))],
        unconditional: false,
    };
    let snap = cat.snapshot();
    let err = validate(&Request::Update(update), &snap).unwrap_err();
    assert_eq!(err.category(), ErrorCategory::Validation);
}
