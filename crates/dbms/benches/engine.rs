//! Criterion bench suite for the engine (`PLAN.md` §3.8), driven through the
//! public `otf_dbms` API on an in-memory backend so the numbers reflect the
//! engine (CoW B+tree, MVCC commit, planner, executor) and not disk latency.
//!
//! Covered: point read (PK point lookup + non-PK equality), full scan, serial
//! vs batch insert, guarded relative update, three-table INNER join + GROUP BY,
//! and standalone GROUP BY. Run with `cargo bench -p otf-dbms --bench engine`;
//! compare runs with criterion's saved baselines (`--save-baseline` /
//! `--baseline`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use otf_dbms::{
    AggFunc, ArithOp, CmpOp, ColumnDef, Database, Expr, IndexDef, Insert, MemoryBackend,
    Projection, Request, Select, Selector, Stage, TableDef, TableRef, TypeKind, Update, Value,
};

// --- tiny AST builders (mirroring the acceptance tests) ----------------------

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

fn row(pairs: &[(&str, i64)]) -> Vec<(String, Value)> {
    pairs
        .iter()
        .map(|(c, v)| (c.to_string(), Value::I64(*v)))
        .collect()
}

fn insert_rows(db: &Database<MemoryBackend>, table: &str, rows: Vec<Vec<(String, Value)>>) {
    db.execute(&Request::Insert(Insert {
        table: table.into(),
        rows,
    }))
    .unwrap();
}

// --- fixtures ----------------------------------------------------------------

/// `users(id pk, val)` seeded with `n` rows (`val = id * 2`).
fn users_db(n: i64) -> Database<MemoryBackend> {
    let db = Database::create_memory().unwrap();
    db.create_table(TableDef::new(
        "users",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("val", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    ))
    .unwrap();
    insert_rows(
        &db,
        "users",
        (0..n)
            .map(|id| row(&[("id", id), ("val", id * 2)]))
            .collect(),
    );
    db
}

// --- reads -------------------------------------------------------------------

fn point_read(c: &mut Criterion) {
    let n = 100_000;
    let mut group = c.benchmark_group("point_read");

    // (1) Primary-key equality → a base-tree point lookup (`Plan::PkLookup`),
    // O(log n). The planner recognizes a full-PK equality and the executor
    // serves it with a single `get`.
    let db = users_db(n);
    let pk_eq = Request::Select(Select::Pipeline(vec![
        Stage::Scan(tref("users")),
        Stage::Match(cmp(CmpOp::Eq, col("id"), lit(n / 2))),
    ]));
    group.bench_function("pk_lookup", |b| {
        b.iter(|| black_box(db.execute(black_box(&pk_eq)).unwrap().len()));
    });

    // (2) Equality on an indexed *non-PK* column → a real index-tree seek
    // (`Plan::IndexScan` served by `CatSnapshot::index_candidates`): the index
    // is range-probed for the matching primary keys, then the base rows are
    // fetched — O(log n + matches).
    let db2 = users_db(n);
    db2.create_index("users", IndexDef::new("by_val", vec!["val"]).unique())
        .unwrap();
    let sec_eq = Request::Select(Select::Pipeline(vec![
        Stage::Scan(tref("users")),
        Stage::Match(cmp(CmpOp::Eq, col("val"), lit((n / 2) * 2))),
    ]));
    group.bench_function("secondary_eq", |b| {
        b.iter(|| black_box(db2.execute(black_box(&sec_eq)).unwrap().len()));
    });

    group.finish();
}

fn full_scan(c: &mut Criterion) {
    // Scan + return every row: raw row decode/materialization throughput.
    let n = 100_000;
    let db = users_db(n);
    let q = Request::Select(Select::Pipeline(vec![Stage::Scan(tref("users"))]));
    let mut group = c.benchmark_group("full_scan");
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function("100k", |b| {
        b.iter(|| black_box(db.execute(black_box(&q)).unwrap().len()));
    });
    group.finish();
}

// --- writes ------------------------------------------------------------------

fn inserts(c: &mut Criterion) {
    // Insert K rows into a fresh empty table two ways: one row per transaction
    // (K commits) vs all K in a single request (one commit). The ratio is the
    // per-commit overhead — the "with/without group commit" comparison (§3.8),
    // here across transaction boundaries rather than fsync batching (the backend
    // is in-memory). Setup builds an empty table, so it barely counts.
    let k = 1_000_i64;
    let mut group = c.benchmark_group("insert_1k");
    group.throughput(Throughput::Elements(k as u64));

    group.bench_function("serial", |b| {
        b.iter_batched(
            || users_db(0),
            |db| {
                for id in 0..k {
                    insert_rows(&db, "users", vec![row(&[("id", id), ("val", id)])]);
                }
                black_box(db);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function("batch", |b| {
        b.iter_batched(
            || users_db(0),
            |db| {
                insert_rows(
                    &db,
                    "users",
                    (0..k).map(|id| row(&[("id", id), ("val", id)])).collect(),
                );
                black_box(db);
            },
            criterion::BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn guarded_update(c: &mut Criterion) {
    // A guarded relative update (`balance = balance + 1 WHERE id = 1 AND
    // balance >= 0`) exercises the read-check-write path evaluated inside the
    // writer. It always matches, so the row survives across iterations.
    let db = Database::create_memory().unwrap();
    db.create_table(TableDef::new(
        "accounts",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("balance", TypeKind::I64)
                .not_null()
                .guarded(),
        ],
        vec!["id"],
    ))
    .unwrap();
    insert_rows(&db, "accounts", vec![row(&[("id", 1), ("balance", 0)])]);
    let bump = Request::Update(Update {
        table: "accounts".into(),
        selector: Some(Selector::Where(Expr::And(vec![
            cmp(CmpOp::Eq, col("id"), lit(1)),
            cmp(CmpOp::Gte, col("balance"), lit(0)),
        ]))),
        set: vec![(
            "balance".into(),
            Expr::Arith {
                op: ArithOp::Add,
                lhs: Box::new(col("balance")),
                rhs: Box::new(lit(1)),
            },
        )],
        unconditional: false,
    });
    c.bench_function("guarded_update", |b| {
        b.iter(|| {
            let out = db.execute(black_box(&bump)).unwrap();
            black_box(out.affected());
        });
    });
}

// --- joins & aggregation -----------------------------------------------------

/// region ⋈ dept ⋈ emp with the given fan-out, all keyed by `id`.
fn join_db(regions: i64, depts: i64, emps: i64) -> Database<MemoryBackend> {
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
    insert_rows(
        &db,
        "region",
        (0..regions).map(|id| row(&[("id", id)])).collect(),
    );
    insert_rows(
        &db,
        "dept",
        (0..depts)
            .map(|id| row(&[("id", id), ("region_id", id % regions)]))
            .collect(),
    );
    insert_rows(
        &db,
        "emp",
        (0..emps)
            .map(|id| row(&[("id", id), ("dept_id", id % depts)]))
            .collect(),
    );
    db
}

fn inner_join_group(c: &mut Criterion) {
    let db = join_db(5, 50, 2_000);
    // emp ⋈ dept ⋈ region, grouped by region, counting employees.
    let q = Request::Select(Select::Pipeline(vec![
        Stage::Scan(tref("emp")),
        Stage::Join(otf_dbms::JoinSpec {
            kind: otf_dbms::JoinKind::Inner,
            table: tref("dept"),
            on: Some(cmp(CmpOp::Eq, qcol("emp", "dept_id"), qcol("dept", "id"))),
        }),
        Stage::Join(otf_dbms::JoinSpec {
            kind: otf_dbms::JoinKind::Inner,
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
    ]));
    c.bench_function("inner_join_group_2k", |b| {
        b.iter(|| {
            let out = db.execute(black_box(&q)).unwrap();
            black_box(out.len())
        });
    });
}

fn group_by(c: &mut Criterion) {
    // sales(id pk, region, amount) with 50k rows over 20 regions; SUM by region.
    let n = 50_000_i64;
    let regions = 20_i64;
    let db = Database::create_memory().unwrap();
    db.create_table(TableDef::new(
        "sales",
        vec![
            ColumnDef::new("id", TypeKind::I64),
            ColumnDef::new("region", TypeKind::I64).not_null(),
            ColumnDef::new("amount", TypeKind::I64).not_null(),
        ],
        vec!["id"],
    ))
    .unwrap();
    insert_rows(
        &db,
        "sales",
        (0..n)
            .map(|id| row(&[("id", id), ("region", id % regions), ("amount", id % 100)]))
            .collect(),
    );
    let q = Request::Select(Select::Pipeline(vec![
        Stage::Scan(tref("sales")),
        Stage::Group {
            by: vec![col("region")],
            aggs: vec![(
                "total".into(),
                Expr::Agg {
                    func: AggFunc::Sum,
                    arg: Box::new(col("amount")),
                },
            )],
        },
        Stage::Project(vec![
            Projection::Expr(col("region")),
            Projection::Expr(col("total")),
        ]),
    ]));
    let mut group = c.benchmark_group("group_by");
    group.throughput(Throughput::Elements(n as u64));
    group.bench_function("sum_50k_over_20", |b| {
        b.iter(|| {
            let out = db.execute(black_box(&q)).unwrap();
            black_box(out.len())
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    point_read,
    full_scan,
    inserts,
    guarded_update,
    inner_join_group,
    group_by,
);
criterion_main!(benches);
