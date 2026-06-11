//! Phase 8 exit criteria, part 1: **decode round-trips for every node** of
//! the grammar, and oversized/over-deep/out-of-grammar messages are rejected
//! with typed errors.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proto::{
    decode_cursor_token, decode_doc, decode_request, encode_cursor_token, encode_doc,
    encode_error_result, encode_request, encode_result, AggFunc, ArithOp, ClauseSelect, CmpOp,
    DecodeLimits, Delete, Dir, Doc, Expr, Insert, JoinKind, JoinSpec, Projection, ProtoError,
    QueryResult, Request, Select, Selector, SortKey, Stage, TableRef, Update,
};
use types::{TypeKind, Value};

fn limits() -> DecodeLimits {
    DecodeLimits::default()
}

/// AST → bytes → AST is the identity.
fn rt(request: &Request) {
    let bytes = encode_request(request);
    let decoded = decode_request(&bytes, &limits()).expect("canonical bytes must decode");
    assert_eq!(&decoded, request);
}

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

fn b(e: Expr) -> Box<Expr> {
    Box::new(e)
}

fn select(stages: Vec<Stage>) -> Request {
    Request::Select(Select::Pipeline(stages))
}

fn scan(table: &str) -> Stage {
    Stage::Scan(TableRef {
        table: table.to_string(),
        alias: None,
    })
}

#[test]
fn every_expression_node_round_trips() {
    let exprs = vec![
        col("a"),
        qcol("u", "name"),
        Expr::Literal(Value::Null),
        Expr::Literal(Value::Bool(true)),
        Expr::Literal(Value::I64(-42)),
        Expr::Literal(Value::F64(2.5)),
        Expr::Literal(Value::Text("s".to_string())),
        Expr::Literal(Value::Blob(vec![1, 2, 3])),
        Expr::Cmp {
            op: CmpOp::Eq,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Cmp {
            op: CmpOp::Ne,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Cmp {
            op: CmpOp::Lt,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Cmp {
            op: CmpOp::Lte,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Cmp {
            op: CmpOp::Gt,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Cmp {
            op: CmpOp::Gte,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::And(vec![col("x"), col("y")]),
        Expr::Or(vec![col("x"), col("y"), col("z")]),
        Expr::Not(b(col("x"))),
        Expr::Arith {
            op: ArithOp::Add,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Arith {
            op: ArithOp::Sub,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Arith {
            op: ArithOp::Mul,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Arith {
            op: ArithOp::Div,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::Arith {
            op: ArithOp::Mod,
            lhs: b(col("a")),
            rhs: b(lit(1)),
        },
        Expr::IsNull(b(col("a"))),
        Expr::IsNotNull(b(col("a"))),
        Expr::Between {
            expr: b(col("a")),
            lo: b(lit(1)),
            hi: b(lit(9)),
        },
        Expr::InList {
            expr: b(col("a")),
            list: vec![lit(1), lit(2), lit(3)],
        },
        Expr::Like {
            expr: b(col("a")),
            pattern: "abc%".to_string(),
            case_insensitive: false,
        },
        Expr::Like {
            expr: b(col("a")),
            pattern: "a_c%".to_string(),
            case_insensitive: true,
        },
        Expr::Coalesce(vec![col("a"), lit(0)]),
        Expr::NullIf {
            lhs: b(col("a")),
            rhs: b(lit(0)),
        },
        Expr::Agg {
            func: AggFunc::Count,
            arg: b(lit(1)),
        },
        Expr::Agg {
            func: AggFunc::Sum,
            arg: b(col("amount")),
        },
        Expr::Agg {
            func: AggFunc::Min,
            arg: b(col("a")),
        },
        Expr::Agg {
            func: AggFunc::Max,
            arg: b(col("a")),
        },
        Expr::Agg {
            func: AggFunc::Avg,
            arg: b(col("a")),
        },
    ];
    // Every cast target type.
    let casts = [
        TypeKind::Bool,
        TypeKind::I64,
        TypeKind::F64,
        TypeKind::Text,
        TypeKind::Blob,
        TypeKind::Uuid,
        TypeKind::Json,
        TypeKind::Timestamp,
    ]
    .map(|to| Expr::Cast {
        expr: b(col("a")),
        to,
    });
    for expr in exprs.into_iter().chain(casts) {
        rt(&select(vec![scan("t"), Stage::Match(expr)]));
    }
}

#[test]
fn every_stage_round_trips() {
    rt(&select(vec![
        Stage::Scan(TableRef {
            table: "users".to_string(),
            alias: Some("u".to_string()),
        }),
        Stage::Match(Expr::Cmp {
            op: CmpOp::Gt,
            lhs: b(qcol("u", "age")),
            rhs: b(lit(18)),
        }),
        Stage::Join(JoinSpec {
            kind: JoinKind::Inner,
            table: TableRef {
                table: "orders".to_string(),
                alias: Some("o".to_string()),
            },
            on: Some(Expr::Cmp {
                op: CmpOp::Eq,
                lhs: b(qcol("u", "id")),
                rhs: b(qcol("o", "user_id")),
            }),
        }),
        Stage::Join(JoinSpec {
            kind: JoinKind::Left,
            table: TableRef {
                table: "notes".to_string(),
                alias: None,
            },
            on: Some(Expr::Cmp {
                op: CmpOp::Eq,
                lhs: b(qcol("u", "id")),
                rhs: b(col("user_id")),
            }),
        }),
        Stage::Join(JoinSpec {
            kind: JoinKind::Cross,
            table: TableRef {
                table: "tags".to_string(),
                alias: None,
            },
            on: None,
        }),
        Stage::Group {
            by: vec![qcol("u", "id")],
            aggs: vec![
                (
                    "spent".to_string(),
                    Expr::Agg {
                        func: AggFunc::Sum,
                        arg: b(qcol("o", "amount")),
                    },
                ),
                (
                    "n".to_string(),
                    Expr::Agg {
                        func: AggFunc::Count,
                        arg: b(lit(1)),
                    },
                ),
            ],
        },
        Stage::Match(Expr::Cmp {
            op: CmpOp::Gt,
            lhs: b(col("spent")),
            rhs: b(lit(100)),
        }),
        Stage::Sort(vec![
            SortKey {
                expr: col("spent"),
                dir: Dir::Desc,
            },
            SortKey {
                expr: qcol("u", "id"),
                dir: Dir::Asc,
            },
        ]),
        Stage::Project(vec![
            Projection::Expr(qcol("u", "name")),
            Projection::Aliased {
                name: "total".to_string(),
                expr: col("spent"),
            },
        ]),
        Stage::Distinct(true),
        Stage::Limit {
            limit: Some(20),
            offset: 40,
        },
        Stage::Cursor(encode_cursor_token(b"pos")),
    ]));
    // Variants: distinct off, limit without offset, offset without limit.
    rt(&select(vec![scan("t"), Stage::Distinct(false)]));
    rt(&select(vec![
        scan("t"),
        Stage::Limit {
            limit: None,
            offset: 7,
        },
    ]));
}

#[test]
fn the_clause_form_round_trips() {
    let clause = ClauseSelect {
        from: Some(TableRef {
            table: "users".to_string(),
            alias: Some("u".to_string()),
        }),
        joins: vec![JoinSpec {
            kind: JoinKind::Inner,
            table: TableRef {
                table: "orders".to_string(),
                alias: Some("o".to_string()),
            },
            on: Some(Expr::Cmp {
                op: CmpOp::Eq,
                lhs: b(qcol("u", "id")),
                rhs: b(qcol("o", "user_id")),
            }),
        }],
        where_: Some(Expr::IsNotNull(b(qcol("u", "email")))),
        group_by: vec![qcol("u", "id")],
        having: Some(Expr::Cmp {
            op: CmpOp::Gt,
            lhs: b(col("spent")),
            rhs: b(lit(100)),
        }),
        order_by: vec![SortKey {
            expr: col("spent"),
            dir: Dir::Desc,
        }],
        select: Some(vec![
            Projection::Expr(qcol("u", "name")),
            Projection::Aliased {
                name: "spent".to_string(),
                expr: Expr::Agg {
                    func: AggFunc::Sum,
                    arg: b(qcol("o", "amount")),
                },
            },
        ]),
        distinct: true,
        limit: Some(20),
        offset: Some(0),
        cursor: Some(encode_cursor_token(b"k")),
    };
    rt(&Request::Select(Select::Clause(Box::new(clause))));
    // Minimal clause: just from.
    rt(&Request::Select(Select::Clause(Box::new(ClauseSelect {
        from: Some(TableRef {
            table: "t".to_string(),
            alias: None,
        }),
        ..ClauseSelect::default()
    }))));
}

#[test]
fn dml_round_trips() {
    rt(&Request::Insert(Insert {
        table: "users".to_string(),
        rows: vec![
            vec![
                ("first_name".to_string(), Value::Text("Ada".to_string())),
                ("age".to_string(), Value::I64(36)),
                ("score".to_string(), Value::F64(0.5)),
                ("active".to_string(), Value::Bool(true)),
                ("bio".to_string(), Value::Null),
                ("avatar".to_string(), Value::Blob(vec![9, 9])),
                // A nested document becomes a json value.
                (
                    "data".to_string(),
                    Value::Json(encode_doc(&Doc::Map(vec![(
                        "role".to_string(),
                        Doc::Str("admin".to_string()),
                    )]))),
                ),
            ],
            vec![("first_name".to_string(), Value::Text("Bo".to_string()))],
        ],
    }));
    // Guarded relative update.
    rt(&Request::Update(Update {
        table: "accounts".to_string(),
        selector: Some(Selector::Where(Expr::And(vec![
            Expr::Cmp {
                op: CmpOp::Eq,
                lhs: b(col("id")),
                rhs: b(lit(7)),
            },
            Expr::Cmp {
                op: CmpOp::Gte,
                lhs: b(col("balance")),
                rhs: b(lit(50)),
            },
        ]))),
        set: vec![(
            "balance".to_string(),
            Expr::Arith {
                op: ArithOp::Sub,
                lhs: b(col("balance")),
                rhs: b(lit(50)),
            },
        )],
        unconditional: false,
    }));
    // Explicit blind set; explicit all-rows selector.
    rt(&Request::Update(Update {
        table: "users".to_string(),
        selector: Some(Selector::All),
        set: vec![("flag".to_string(), lit(0))],
        unconditional: true,
    }));
    // Missing selector decodes faithfully (the validator rejects it later).
    rt(&Request::Update(Update {
        table: "users".to_string(),
        selector: None,
        set: vec![("flag".to_string(), lit(1))],
        unconditional: false,
    }));
    rt(&Request::Delete(Delete {
        table: "sessions".to_string(),
        selector: Some(Selector::Where(Expr::Cmp {
            op: CmpOp::Lt,
            lhs: b(col("id")),
            rhs: b(lit(1000)),
        })),
    }));
    rt(&Request::Delete(Delete {
        table: "sessions".to_string(),
        selector: Some(Selector::All),
    }));
}

#[test]
fn transaction_and_explain_round_trip() {
    rt(&Request::Transaction(vec![
        Request::Update(Update {
            table: "a".to_string(),
            selector: Some(Selector::Where(Expr::Cmp {
                op: CmpOp::Eq,
                lhs: b(col("id")),
                rhs: b(lit(1)),
            })),
            set: vec![("v".to_string(), lit(1))],
            unconditional: false,
        }),
        Request::Insert(Insert {
            table: "log".to_string(),
            rows: vec![vec![("msg".to_string(), Value::Text("x".to_string()))]],
        }),
        Request::Delete(Delete {
            table: "tmp".to_string(),
            selector: Some(Selector::All),
        }),
    ]));
    rt(&Request::Explain(Select::Pipeline(vec![
        scan("t"),
        Stage::Match(Expr::IsNull(b(col("a")))),
    ])));
}

// --- grammar rejection -------------------------------------------------------

fn doc_bytes(doc: &Doc) -> Vec<u8> {
    encode_doc(doc)
}

fn req_map(entries: Vec<(&str, Doc)>) -> Vec<u8> {
    doc_bytes(&Doc::Map(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    ))
}

fn s(v: &str) -> Doc {
    Doc::Str(v.to_string())
}

#[test]
fn unknown_nodes_and_fields_are_rejected() {
    // Unknown op.
    let bytes = req_map(vec![("op", s("drop_table"))]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::UnknownNode {
            context: "request",
            ..
        })
    ));
    // Unknown stage.
    let bytes = req_map(vec![
        ("op", s("select")),
        (
            "pipeline",
            Doc::Array(vec![Doc::Map(vec![("explode".to_string(), Doc::Null)])]),
        ),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::UnknownNode {
            context: "stage",
            ..
        })
    ));
    // Unknown expression node ("exec" is not in the grammar — data, not code).
    let bytes = req_map(vec![
        ("op", s("select")),
        ("from", Doc::Map(vec![("table".to_string(), s("t"))])),
        ("where", Doc::Map(vec![("exec".to_string(), s("rm -rf /"))])),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::UnknownNode {
            context: "expression",
            ..
        })
    ));
    // Unknown field on a known node.
    let bytes = req_map(vec![
        ("op", s("delete")),
        ("table", s("t")),
        ("all", Doc::Bool(true)),
        ("force", Doc::Bool(true)),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::UnknownField { node: "delete", .. })
    ));
    // A select inside a transaction.
    let bytes = req_map(vec![
        ("op", s("transaction")),
        (
            "ops",
            Doc::Array(vec![Doc::Map(vec![("op".to_string(), s("select"))])]),
        ),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::UnknownNode {
            context: "transaction op",
            ..
        })
    ));
}

#[test]
fn versioning_is_enforced() {
    // Explicit v:1 is fine.
    let bytes = req_map(vec![
        ("v", Doc::Int(1)),
        ("op", s("delete")),
        ("table", s("t")),
        ("all", Doc::Bool(true)),
    ]);
    assert!(decode_request(&bytes, &limits()).is_ok());
    // v:2 is not.
    let bytes = req_map(vec![
        ("v", Doc::Int(2)),
        ("op", s("delete")),
        ("table", s("t")),
        ("all", Doc::Bool(true)),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::UnsupportedVersion { found: 2 })
    ));
}

#[test]
fn structural_grammar_violations_are_rejected() {
    // where and all together.
    let bytes = req_map(vec![
        ("op", s("delete")),
        ("table", s("t")),
        (
            "where",
            Doc::Map(vec![(
                "is_null".to_string(),
                Doc::Map(vec![("col".to_string(), s("a"))]),
            )]),
        ),
        ("all", Doc::Bool(true)),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::InvalidValue { field: "all", .. })
    ));
    // all:false is meaningless.
    let bytes = req_map(vec![
        ("op", s("delete")),
        ("table", s("t")),
        ("all", Doc::Bool(false)),
    ]);
    assert!(decode_request(&bytes, &limits()).is_err());
    // Inner join without on; cross join with on.
    let join = |kind: &str, with_on: bool| {
        let mut entries = vec![
            ("type".to_string(), s(kind)),
            (
                "table".to_string(),
                Doc::Map(vec![("table".to_string(), s("o"))]),
            ),
        ];
        if with_on {
            entries.push((
                "on".to_string(),
                Doc::Map(vec![("col".to_string(), s("x"))]),
            ));
        }
        req_map(vec![
            ("op", s("select")),
            ("from", Doc::Map(vec![("table".to_string(), s("t"))])),
            ("joins", Doc::Array(vec![Doc::Map(entries)])),
        ])
    };
    assert!(matches!(
        decode_request(&join("inner", false), &limits()),
        Err(ProtoError::MissingField { field: "on", .. })
    ));
    assert!(decode_request(&join("cross", true), &limits()).is_err());
    assert!(decode_request(&join("cross", false), &limits()).is_ok());
    // Bad enum values.
    let bytes = req_map(vec![
        ("op", s("select")),
        ("from", Doc::Map(vec![("table".to_string(), s("t"))])),
        (
            "order_by",
            Doc::Array(vec![Doc::Map(vec![
                (
                    "expr".to_string(),
                    Doc::Map(vec![("col".to_string(), s("a"))]),
                ),
                ("dir".to_string(), s("sideways")),
            ])]),
        ),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::InvalidValue { field: "dir", .. })
    ));
    // Bad cast target.
    let bytes = req_map(vec![
        ("op", s("select")),
        ("from", Doc::Map(vec![("table".to_string(), s("t"))])),
        (
            "where",
            Doc::Map(vec![(
                "cast".to_string(),
                Doc::Array(vec![
                    Doc::Map(vec![("col".to_string(), s("a"))]),
                    s("decimal"),
                ]),
            )]),
        ),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::InvalidValue {
            field: "cast type",
            ..
        })
    ));
    // Pipeline and clause keys cannot mix.
    let bytes = req_map(vec![
        ("op", s("select")),
        ("pipeline", Doc::Array(vec![])),
        ("from", Doc::Map(vec![("table".to_string(), s("t"))])),
    ]);
    assert!(matches!(
        decode_request(&bytes, &limits()),
        Err(ProtoError::UnknownField { .. })
    ));
}

// --- wire hardening ------------------------------------------------------------

#[test]
fn oversized_messages_are_rejected_before_decoding() {
    let limits = DecodeLimits {
        max_bytes: 16,
        ..DecodeLimits::default()
    };
    let bytes = doc_bytes(&Doc::Str("x".repeat(100)));
    assert!(matches!(
        decode_doc(&bytes, &limits),
        Err(ProtoError::MessageTooLarge { len: 102, max: 16 })
    ));
}

#[test]
fn over_deep_messages_are_rejected() {
    // 70 nested single-item arrays around a nil: beyond the default depth 64.
    let mut bytes = vec![0x91u8; 70];
    bytes.push(0xC0);
    assert!(matches!(
        decode_doc(&bytes, &limits()),
        Err(ProtoError::TooDeep { max: 64 })
    ));
    // 60 deep is fine.
    let mut bytes = vec![0x91u8; 60];
    bytes.push(0xC0);
    assert!(decode_doc(&bytes, &limits()).is_ok());
}

#[test]
fn claimed_giant_containers_do_not_allocate() {
    // array32 claiming u32::MAX items in a 6-byte message: must be rejected
    // by the pre-allocation count check, not by attempting the allocation.
    let mut bytes = vec![0xDD];
    bytes.extend_from_slice(&u32::MAX.to_be_bytes());
    bytes.push(0xC0);
    assert!(matches!(
        decode_doc(&bytes, &limits()),
        Err(ProtoError::Truncated | ProtoError::TooManyNodes)
    ));
    // map16 claiming 0xFFFF entries.
    let mut bytes = vec![0xDE, 0xFF, 0xFF];
    bytes.push(0xC0);
    assert!(decode_doc(&bytes, &limits()).is_err());
}

#[test]
fn the_node_budget_is_enforced() {
    let limits = DecodeLimits {
        max_nodes: 10,
        ..DecodeLimits::default()
    };
    // An array of 20 small ints: 21 nodes > 10.
    let doc = Doc::Array((0..20).map(Doc::Int).collect());
    assert!(matches!(
        decode_doc(&doc_bytes(&doc), &limits),
        Err(ProtoError::TooManyNodes)
    ));
    // 9 ints fit.
    let doc = Doc::Array((0..9).map(Doc::Int).collect());
    assert!(decode_doc(&doc_bytes(&doc), &limits).is_ok());
}

#[test]
fn malformed_wire_bytes_are_rejected() {
    let cases: Vec<(&[u8], &str)> = vec![
        (&[0x91], "truncated array"),
        (&[0xC0, 0xC0], "trailing bytes"),
        (&[0xC1], "reserved byte"),
        (&[0xD4, 0x00, 0x00], "fixext"),
        (&[0xC7, 0x01, 0x00, 0x00], "ext8"),
        (&[0xA1, 0xFF], "invalid utf-8"),
        (&[0x81, 0x01, 0xC0], "non-string map key"),
        (&[0x82, 0xA1, b'a', 0xC0, 0xA1, b'a', 0xC0], "duplicate key"),
        (
            &[0xCF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            "u64 beyond i64",
        ),
        (&[], "empty input"),
    ];
    for (bytes, what) in cases {
        assert!(decode_doc(bytes, &limits()).is_err(), "{what} must fail");
    }
}

#[test]
fn docs_round_trip_through_the_wire_layer() {
    let doc = Doc::Map(vec![
        ("null".to_string(), Doc::Null),
        ("b".to_string(), Doc::Bool(true)),
        ("neg".to_string(), Doc::Int(-129)),
        ("big".to_string(), Doc::Int(i64::MAX)),
        ("small".to_string(), Doc::Int(i64::MIN)),
        ("f".to_string(), Doc::Float(-0.125)),
        ("s".to_string(), Doc::Str("héllo".to_string())),
        ("bin".to_string(), Doc::Bin(vec![0, 255, 7])),
        (
            "arr".to_string(),
            Doc::Array(vec![Doc::Int(1), Doc::Str("two".to_string())]),
        ),
        (
            "nested".to_string(),
            Doc::Map(vec![("k".to_string(), Doc::Array(vec![Doc::Null]))]),
        ),
    ]);
    assert_eq!(decode_doc(&encode_doc(&doc), &limits()).unwrap(), doc);
    // A long string exercising the str16 header.
    let doc = Doc::Str("x".repeat(40_000));
    assert_eq!(decode_doc(&encode_doc(&doc), &limits()).unwrap(), doc);
}

// --- results & cursor tokens -----------------------------------------------------

#[test]
fn results_encode_to_the_spec_shape() {
    let result = QueryResult {
        columns: vec!["name".to_string(), "spent".to_string()],
        rows: vec![
            vec![Value::Text("Ada".to_string()), Value::I64(120)],
            vec![Value::Null, Value::I64(7)],
        ],
        cursor: Some(encode_cursor_token(b"next")),
        applied: Some(true),
        affected: Some(2),
    };
    let bytes = encode_result(&result).unwrap();
    let doc = decode_doc(&bytes, &limits()).unwrap();
    assert_eq!(doc.get("v"), Some(&Doc::Int(1)));
    assert_eq!(doc.get("ok"), Some(&Doc::Bool(true)));
    assert_eq!(
        doc.get("columns"),
        Some(&Doc::Array(vec![s("name"), s("spent")]))
    );
    assert_eq!(
        doc.get("rows"),
        Some(&Doc::Array(vec![
            Doc::Array(vec![s("Ada"), Doc::Int(120)]),
            Doc::Array(vec![Doc::Null, Doc::Int(7)]),
        ]))
    );
    assert!(matches!(doc.get("cursor"), Some(Doc::Bin(_))));
    assert_eq!(doc.get("applied"), Some(&Doc::Bool(true)));
    assert_eq!(doc.get("affected"), Some(&Doc::Int(2)));

    // Optional fields are explicit nulls.
    let bytes = encode_result(&QueryResult::default()).unwrap();
    let doc = decode_doc(&bytes, &limits()).unwrap();
    assert_eq!(doc.get("cursor"), Some(&Doc::Null));
    assert_eq!(doc.get("applied"), Some(&Doc::Null));
    assert_eq!(doc.get("affected"), Some(&Doc::Null));
}

#[test]
fn error_results_carry_the_category_code() {
    let bytes = encode_error_result(common::ErrorCategory::Constraint, "unique violation");
    let doc = decode_doc(&bytes, &limits()).unwrap();
    assert_eq!(doc.get("ok"), Some(&Doc::Bool(false)));
    assert_eq!(doc.get("code"), Some(&s("constraint")));
    assert_eq!(doc.get("error"), Some(&s("unique violation")));
}

#[test]
fn cursor_tokens_reject_tampering() {
    let token = encode_cursor_token(b"keyset position");
    assert_eq!(decode_cursor_token(&token).unwrap(), b"keyset position");

    // Flip each byte in turn: every mangled token must be rejected.
    for i in 0..token.len() {
        let mut bad = token.clone();
        bad[i] ^= 0x40;
        assert!(
            matches!(decode_cursor_token(&bad), Err(ProtoError::BadCursorToken)),
            "flipped byte {i} must invalidate the token"
        );
    }
    // Truncations too.
    for len in 0..token.len() {
        assert!(decode_cursor_token(&token[..len]).is_err());
    }
    // The empty payload still round-trips.
    assert_eq!(decode_cursor_token(&encode_cursor_token(b"")).unwrap(), b"");
}
