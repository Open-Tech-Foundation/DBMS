//! Phase 8 exit criteria, part 3: **clause ↔ pipeline equivalence** — the
//! same query written in either surface form lowers to the identical IR
//! plan, hand-built cases plus a seeded random sweep.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use common::{Rng, SeededRng};
use proto::{
    AggFunc, ClauseSelect, CmpOp, Dir, Expr, JoinKind, JoinSpec, Plan, Projection, Select, SortKey,
    Stage, TableRef,
};
use query::{lower, LowerError};
use types::Value;

fn col(name: &str) -> Expr {
    Expr::Column {
        table: None,
        column: name.to_string(),
    }
}

fn qcol(table: &str, name: &str) -> Expr {
    Expr::Column {
        table: Some(table.to_string()),
        column: name.to_string(),
    }
}

fn lit(v: i64) -> Expr {
    Expr::Literal(Value::I64(v))
}

fn eq(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Cmp {
        op: CmpOp::Eq,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn gt(lhs: Expr, rhs: Expr) -> Expr {
    Expr::Cmp {
        op: CmpOp::Gt,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn tref(table: &str, alias: Option<&str>) -> TableRef {
    TableRef {
        table: table.to_string(),
        alias: alias.map(str::to_string),
    }
}

fn sum(arg: Expr) -> Expr {
    Expr::Agg {
        func: AggFunc::Sum,
        arg: Box::new(arg),
    }
}

fn count_rows() -> Expr {
    Expr::Agg {
        func: AggFunc::Count,
        arg: Box::new(lit(1)),
    }
}

/// The `SPEC.md` §5.3/§5.4 worked example, written in both forms.
#[test]
fn the_spec_example_lowers_identically_from_both_surfaces() {
    let on = eq(qcol("u", "id"), qcol("o", "user_id"));
    let join = JoinSpec {
        kind: JoinKind::Inner,
        table: tref("orders", Some("o")),
        on: Some(on),
    };
    let having = gt(col("spent"), lit(100));
    let order = SortKey {
        expr: col("spent"),
        dir: Dir::Desc,
    };

    let pipeline = Select::Pipeline(vec![
        Stage::Scan(tref("users", Some("u"))),
        Stage::Join(join.clone()),
        Stage::Group {
            by: vec![qcol("u", "id")],
            aggs: vec![
                ("spent".to_string(), sum(qcol("o", "amount"))),
                ("n".to_string(), count_rows()),
            ],
        },
        Stage::Match(having.clone()),
        Stage::Project(vec![
            Projection::Expr(qcol("u", "name")),
            Projection::Expr(col("spent")),
            Projection::Expr(col("n")),
        ]),
        Stage::Sort(vec![order.clone()]),
        Stage::Limit {
            limit: Some(20),
            offset: 0,
        },
    ]);

    let clause = Select::Clause(Box::new(ClauseSelect {
        from: Some(tref("users", Some("u"))),
        joins: vec![join],
        group_by: vec![qcol("u", "id")],
        having: Some(having),
        order_by: vec![order],
        select: Some(vec![
            Projection::Expr(qcol("u", "name")),
            Projection::Aliased {
                name: "spent".to_string(),
                expr: sum(qcol("o", "amount")),
            },
            Projection::Aliased {
                name: "n".to_string(),
                expr: count_rows(),
            },
        ]),
        limit: Some(20),
        offset: Some(0),
        ..ClauseSelect::default()
    }));

    let plan = lower(&pipeline).unwrap();
    assert_eq!(lower(&clause).unwrap(), plan);

    // And the plan is the expected tree, leaf to root:
    // scan → join → aggregate → filter(having) → project → sort → limit.
    let Plan::Limit {
        input,
        limit: Some(20),
        offset: 0,
    } = plan
    else {
        panic!("expected limit at the root");
    };
    let Plan::Sort { input, .. } = *input else {
        panic!("expected sort under limit");
    };
    let Plan::Project { input, items } = *input else {
        panic!("expected project under sort");
    };
    assert_eq!(items.len(), 3);
    let Plan::Filter { input, .. } = *input else {
        panic!("expected the having filter under project");
    };
    let Plan::Aggregate { input, by, aggs } = *input else {
        panic!("expected aggregate under having");
    };
    assert_eq!(by, vec![qcol("u", "id")]);
    assert_eq!(aggs[0].0, "spent");
    let Plan::Join {
        kind: JoinKind::Inner,
        left,
        right,
        on: Some(_),
    } = *input
    else {
        panic!("expected join under aggregate");
    };
    assert_eq!(
        *left,
        Plan::Scan {
            table: "users".to_string(),
            alias: Some("u".to_string()),
        }
    );
    assert_eq!(
        *right,
        Plan::Scan {
            table: "orders".to_string(),
            alias: Some("o".to_string()),
        }
    );
}

#[test]
fn simple_filters_and_pagination_are_equivalent() {
    let where_ = gt(col("age"), lit(18));
    let order = SortKey {
        expr: col("age"),
        dir: Dir::Asc,
    };
    let token = proto::encode_cursor_token(b"page2");

    let pipeline = Select::Pipeline(vec![
        Stage::Scan(tref("users", None)),
        Stage::Match(where_.clone()),
        Stage::Project(vec![Projection::Expr(col("name"))]),
        Stage::Distinct(true),
        Stage::Sort(vec![order.clone()]),
        Stage::Limit {
            limit: Some(10),
            offset: 0,
        },
        Stage::Cursor(token.clone()),
    ]);
    let clause = Select::Clause(Box::new(ClauseSelect {
        from: Some(tref("users", None)),
        where_: Some(where_),
        order_by: vec![order],
        select: Some(vec![Projection::Expr(col("name"))]),
        distinct: true,
        limit: Some(10),
        offset: Some(0),
        cursor: Some(token),
        ..ClauseSelect::default()
    }));
    assert_eq!(lower(&pipeline).unwrap(), lower(&clause).unwrap());
}

#[test]
fn unaliased_aggregates_get_their_function_name() {
    let clause = Select::Clause(Box::new(ClauseSelect {
        from: Some(tref("t", None)),
        select: Some(vec![Projection::Expr(count_rows())]),
        ..ClauseSelect::default()
    }));
    let pipeline = Select::Pipeline(vec![
        Stage::Scan(tref("t", None)),
        Stage::Group {
            by: vec![],
            aggs: vec![("count".to_string(), count_rows())],
        },
        Stage::Project(vec![Projection::Expr(col("count"))]),
    ]);
    assert_eq!(lower(&clause).unwrap(), lower(&pipeline).unwrap());
}

#[test]
fn structural_lowering_errors() {
    // Empty pipeline.
    assert_eq!(
        lower(&Select::Pipeline(vec![])),
        Err(LowerError::EmptyPipeline)
    );
    // Pipeline not starting with scan.
    assert_eq!(
        lower(&Select::Pipeline(vec![Stage::Match(lit(1))])),
        Err(LowerError::MissingSource { found: "match" })
    );
    // Scan in the middle.
    assert_eq!(
        lower(&Select::Pipeline(vec![
            Stage::Scan(tref("a", None)),
            Stage::Scan(tref("b", None)),
        ])),
        Err(LowerError::MisplacedScan)
    );
    // A group output that is not an aggregate.
    assert_eq!(
        lower(&Select::Pipeline(vec![
            Stage::Scan(tref("t", None)),
            Stage::Group {
                by: vec![col("a")],
                aggs: vec![("x".to_string(), col("a"))],
            },
        ])),
        Err(LowerError::NotAnAggregate {
            name: "x".to_string()
        })
    );
    // Clause without from.
    assert_eq!(
        lower(&Select::Clause(Box::default())),
        Err(LowerError::MissingFrom)
    );
    // Having without grouping.
    assert_eq!(
        lower(&Select::Clause(Box::new(ClauseSelect {
            from: Some(tref("t", None)),
            having: Some(gt(col("a"), lit(1))),
            ..ClauseSelect::default()
        }))),
        Err(LowerError::HavingWithoutGroup)
    );
    // An aggregate buried in a larger select expression.
    assert_eq!(
        lower(&Select::Clause(Box::new(ClauseSelect {
            from: Some(tref("t", None)),
            select: Some(vec![Projection::Expr(Expr::Arith {
                op: proto::ArithOp::Add,
                lhs: Box::new(count_rows()),
                rhs: Box::new(lit(1)),
            })]),
            ..ClauseSelect::default()
        }))),
        Err(LowerError::NestedAggregate)
    );
    // Two unaliased counts collide.
    assert_eq!(
        lower(&Select::Clause(Box::new(ClauseSelect {
            from: Some(tref("t", None)),
            select: Some(vec![
                Projection::Expr(count_rows()),
                Projection::Expr(count_rows()),
            ]),
            ..ClauseSelect::default()
        }))),
        Err(LowerError::DuplicateAggName {
            name: "count".to_string()
        })
    );
}

#[test]
fn distinct_false_is_a_no_op_stage() {
    let with = lower(&Select::Pipeline(vec![
        Stage::Scan(tref("t", None)),
        Stage::Distinct(false),
    ]))
    .unwrap();
    let without = lower(&Select::Pipeline(vec![Stage::Scan(tref("t", None))])).unwrap();
    assert_eq!(with, without);
}

// --- the seeded random sweep ---------------------------------------------------

/// Build a random clause select and, **independently**, the pipeline a user
/// would write by hand for it (per the documented fixed clause order). Both
/// must lower to the identical plan.
#[test]
fn random_equivalent_pairs_lower_identically() {
    for seed in [1u64, 2, 3, 4, 5, 6, 7, 8] {
        let rng = SeededRng::new(seed);
        for case in 0..250 {
            let (clause, pipeline) = random_pair(&rng);
            let from_clause = lower(&Select::Clause(Box::new(clause.clone())));
            let from_pipeline = lower(&Select::Pipeline(pipeline.clone()));
            match (from_clause, from_pipeline) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "seed {seed} case {case}: plans diverged\n{clause:?}");
                }
                (a, b) => panic!("seed {seed} case {case}: lowering failed: {a:?} / {b:?}"),
            }
        }
    }
}

fn random_pair(rng: &SeededRng) -> (ClauseSelect, Vec<Stage>) {
    let pick = |n: u64| rng.next_u64() % n;
    let cols = ["a", "b", "c", "d"];
    let rand_col = |rng: &SeededRng| col(cols[(rng.next_u64() % 4) as usize]);
    let rand_pred = |rng: &SeededRng| match rng.next_u64() % 3 {
        0 => gt(rand_col(rng), lit((rng.next_u64() % 100) as i64)),
        1 => Expr::IsNull(Box::new(rand_col(rng))),
        _ => Expr::And(vec![
            eq(rand_col(rng), lit(1)),
            Expr::IsNotNull(Box::new(rand_col(rng))),
        ]),
    };

    let mut clause = ClauseSelect {
        from: Some(tref("base", Some("t"))),
        ..ClauseSelect::default()
    };
    let mut stages = vec![Stage::Scan(tref("base", Some("t")))];

    // Joins (0..=2).
    for j in 0..pick(3) {
        let kind = match pick(3) {
            0 => JoinKind::Inner,
            1 => JoinKind::Left,
            _ => JoinKind::Cross,
        };
        let join = JoinSpec {
            kind,
            table: tref(&format!("j{j}"), None),
            on: if kind == JoinKind::Cross {
                None
            } else {
                Some(eq(qcol("t", "id"), col("fk")))
            },
        };
        clause.joins.push(join.clone());
        stages.push(Stage::Join(join));
    }
    // Where.
    if pick(2) == 0 {
        let pred = rand_pred(rng);
        clause.where_ = Some(pred.clone());
        stages.push(Stage::Match(pred));
    }
    // Grouping (with select-list aggregates) or a plain select list.
    let grouped = pick(2) == 0;
    if grouped {
        let by: Vec<Expr> = (0..=pick(2)).map(|_| rand_col(rng)).collect();
        let n_aggs = 1 + pick(2);
        let mut aggs = Vec::new();
        let mut select_items = Vec::new();
        if let Some(first_key) = by.first() {
            // Project one of the grouping keys too.
            select_items.push(Projection::Expr(first_key.clone()));
        }
        for i in 0..n_aggs {
            let name = format!("agg{i}");
            let agg = if pick(2) == 0 {
                sum(rand_col(rng))
            } else {
                count_rows()
            };
            aggs.push((name.clone(), agg.clone()));
            select_items.push(Projection::Aliased { name, expr: agg });
        }
        clause.group_by = by.clone();
        clause.select = Some(select_items.clone());
        stages.push(Stage::Group { by, aggs });
        // Having.
        if pick(2) == 0 {
            let having = gt(col("agg0"), lit(10));
            clause.having = Some(having.clone());
            stages.push(Stage::Match(having));
        }
        // The pipeline projects the group outputs by name.
        let projected: Vec<Projection> = select_items
            .iter()
            .map(|item| match item {
                Projection::Aliased { name, .. } => Projection::Expr(col(name)),
                Projection::Expr(expr) => Projection::Expr(expr.clone()),
            })
            .collect();
        stages.push(Stage::Project(projected));
    } else if pick(2) == 0 {
        let items: Vec<Projection> = (0..=pick(3))
            .map(|_| Projection::Expr(rand_col(rng)))
            .collect();
        clause.select = Some(items.clone());
        stages.push(Stage::Project(items));
    }
    // Distinct.
    if pick(3) == 0 {
        clause.distinct = true;
        stages.push(Stage::Distinct(true));
    }
    // Order by.
    if pick(2) == 0 {
        let keys = vec![SortKey {
            expr: rand_col(rng),
            dir: if pick(2) == 0 { Dir::Asc } else { Dir::Desc },
        }];
        clause.order_by = keys.clone();
        stages.push(Stage::Sort(keys));
    }
    // Limit / offset.
    match pick(3) {
        0 => {
            clause.limit = Some(pick(50));
            stages.push(Stage::Limit {
                limit: clause.limit,
                offset: 0,
            });
        }
        1 => {
            clause.limit = Some(pick(50));
            clause.offset = Some(pick(50));
            stages.push(Stage::Limit {
                limit: clause.limit,
                offset: clause.offset.unwrap_or(0),
            });
        }
        _ => {}
    }
    // Cursor.
    if pick(4) == 0 {
        let token = proto::encode_cursor_token(&rng.next_u64().to_be_bytes());
        clause.cursor = Some(token.clone());
        stages.push(Stage::Cursor(token));
    }
    (clause, stages)
}
