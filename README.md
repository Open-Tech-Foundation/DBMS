# OTF DBMS

An embedded, single-file relational database written in Rust, with a
structured (non-SQL) binary query interface.

- **ACID, crash-safe storage.** A copy-on-write B+tree over a paged,
  checksummed file with a double-buffered meta page: every commit is atomic and
  durable, and a crash at any point recovers to the last committed state — never
  a torn or partial one.
- **MVCC snapshot isolation.** Readers take a consistent snapshot and are never
  blocked by the single writer; a cursor owns its snapshot for stable pagination
  under concurrent writes.
- **Relational core.** Tables with typed columns, primary keys, secondary
  indexes (with index-aware planning), `NOT NULL` / `UNIQUE` / `CHECK` /
  `DEFAULT` / `FOREIGN KEY` (RESTRICT) constraints, and a validating query
  layer.
- **Safe concurrent writes.** Guarded relative updates and optimistic
  (version-guarded) updates are evaluated inside the single writer, so
  read-check-write is one indivisible step — no TOCTOU, first-committer-wins.
- **Hardened.** Bounded wire decoding, per-query resource caps (rows, joins,
  memory, deadline → a clean `ResourceLimit`), checked arithmetic, and integrity
  tooling that surfaces corruption rather than serving it.

> Scope: this is the storage + query engine. Networking, auth, encryption, and
> a SQL text front-end are explicitly out of scope for v1 (see `docs/PLAN.md` §8).

## Quick start

```rust
use otf_dbms::{
    ColumnDef, Database, Insert, Request, Select, Stage, TableDef, TableRef, TypeKind, Value,
};

// An in-memory database (use `Database::create` / `open` for a file).
let db = Database::create_memory()?;

// Define a table.
db.create_table(TableDef::new(
    "users",
    vec![
        ColumnDef::new("id", TypeKind::I64),
        ColumnDef::new("name", TypeKind::Text).not_null(),
    ],
    vec!["id"], // primary key
))?;

// Insert a row.
db.execute(&Request::Insert(Insert {
    table: "users".into(),
    rows: vec![vec![
        ("id".into(), Value::I64(1)),
        ("name".into(), Value::Text("Ada".into())),
    ]],
}))?;

// Query it back.
let out = db.execute(&Request::Select(Select::Pipeline(vec![Stage::Scan(
    TableRef { table: "users".into(), alias: None },
)])))?;
assert_eq!(out.row(0).unwrap().get_text("name")?, Some("Ada"));
# Ok::<(), otf_dbms::Error>(())
```

Requests are built as typed ASTs (as above) or sent as MessagePack bytes via
`Database::execute_wire`. For stable pagination over a large result under a
concurrent writer, open a snapshot-owning cursor with `Database::open_cursor`.

## Command-line tool

```text
otf-dbms check   <file>   # full integrity check (pager + trees + indexes)
otf-dbms inspect <file>   # structural summary (storage + per-table counts)
otf-dbms repl    <file>   # open (or create) and explore interactively
```

The REPL is an inspection console — `\tables`, `\schema <t>`, `\count <t>`,
`\scan <t> [n]`, `\inspect`, `\check`, `\timing`, `\help`, `\quit`.

## Documentation

- `docs/SPEC.md` — the behavioral contract (types, constraints, safety rules).
- `docs/ARCHITECTURE.md` — the crate stack and how a request flows through it.
- `docs/PLAN.md` — the phased implementation plan and acceptance scenarios.
- `DECISIONS.md` — every design decision and its rationale, newest first.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See the [NOTICE](NOTICE) file for attribution.

```text
OTF DBMS
Copyright 2026 Open Tech Foundation and its contributors
```
