# SPEC — Embedded Relational Database Engine (v1)

> **What this document is.** The behavioral **contract**: the data model, type system, query
> grammar, constraints, safety rules, and the durability/security guarantees — i.e. everything that
> must be true regardless of how the engine is implemented. Internal design lives in
> `ARCHITECTURE.md`; the build sequence lives in `PLAN.md`.
>
> Names in this project are generic. Crate/module names below (`pager`, `btree`, …) are functional
> placeholders; rename when a product name is chosen.

---

## 1. Overview & category

The engine is an **embedded, single-file, ACID/OLTP, row-oriented relational database** with a
**non-SQL structured query interface**. Queries are sent as structured binary data (MessagePack),
not as SQL text. It is delivered as an in-process Rust library, not a server.

Placed against existing systems: same family as SQLite (embedded relational engine), but with
queries expressed as structured data (in the spirit of MongoDB's query documents) rather than SQL
text, and with a single fixed concurrency model.

**Target workloads:** on-device/application-local storage, read-balanced workloads, edge/per-tenant
databases, local-first/offline-sync apps, and queryable index/metadata stores.

**Non-goals (v1):** SQL text parsing, a network server, replication, per-language SDKs, columnar
storage, distributed operation, OLAP/analytics-scale aggregation, high-ingest write streams.

---

## 2. Data model

- Data is organized into **tables** of **rows**; each row is a fixed set of **typed columns**.
- The schema is **strict**: every column has a declared type, and writes that violate the schema
  are rejected.
- Every table has a **required primary key (PK)**, single- or multi-column.
- Relationships are expressed by values (e.g. a column holding another table's key) and resolved at
  query time via joins. **Foreign keys** enforce referential integrity (see §4.1); the `RESTRICT`
  action is enforced today, with `CASCADE` / `SET NULL` modelled but not yet enforced.

---

## 3. Type system (v1)

| Type | Storage | Notes |
|---|---|---|
| `null` | — | The absence of a value; distinct from any other value. |
| `bool` | 1 byte | |
| `i64` | 8 bytes | Signed 64-bit integer. **Use for money** (minor units, e.g. cents). |
| `f64` | 8 bytes | IEEE-754 double. Not for money. |
| `text` | variable | UTF-8. |
| `blob` | variable | Opaque bytes. |
| `uuid` | 16 bytes | Canonical string on I/O. **UUIDv7 is the default generator for key/indexed columns** (time-ordered → near-sequential inserts). |
| `json` | variable | A *document* stored internally as MessagePack. **Opaque in v1** (store/fetch/validate well-formed); no path queries or path indexes until v2. |
| `timestamp` | i64 | Epoch **microseconds, UTC**. Comparable, sortable, indexable. Date arithmetic/formatting functions are v2. |

Notes:
- **Money:** represent as `i64` in minor units in v1. A dedicated `decimal` type is v2.
- **Three-valued logic:** comparisons involving `null` yield `null` (not true/false); `WHERE`
  keeps only rows where the predicate is true. `IS NULL` / `IS NOT NULL` test for null explicitly.

---

## 4. Schema & constraints

### 4.1 Constraints (v1)
- `PRIMARY KEY` — required, unique, not null.
- `NOT NULL`
- `UNIQUE` — enforced via a unique index.
- `CHECK(<expr>)` — a boolean expression over the row; rejected on violation.
- `DEFAULT <value | generator>` — applied when a column is omitted on insert.
- `FOREIGN KEY(cols) REFERENCES parent(cols)` — the referencing columns must match an existing
  parent key. Referenced columns must be the parent's PK or a `UNIQUE` index; composite and
  self-referential keys are allowed; `MATCH SIMPLE` (a NULL in any referencing column skips the
  check). Referential action `RESTRICT` (the default) is enforced on the referenced side (parent
  delete/update and `drop table` are blocked while children exist); `CASCADE` and `SET NULL` are
  reserved but not yet enforced.
- **Auto-increment** integer keys.
- **Generators** usable as defaults: `now` (current timestamp), `uuid_v7` (new time-ordered UUID).
- **`rowversion`** column — engine auto-increments it on every write to the row; usable as an
  optimistic-concurrency guard.
- **`on_update: now`** — a timestamp column the engine refreshes on every update (for `updated_at`).

### 4.2 Per-column update policy
Every column declares `update: free | guarded` (default `free`):
- `free` — a plain absolute set is allowed (last-write-wins). Appropriate for value-independent
  fields (e.g. `first_name`).
- `guarded` — a **blind absolute set is rejected by the validator**; the column may only be written
  via a relative expression (`col = col − x`) or an update carrying a guard/version condition.
  Appropriate for value-dependent fields (e.g. `balance`).

### 4.3 Example schema
```
table accounts {
  id:         { type: uuid,      pk: true, default: uuid_v7 }
  balance:    { type: i64,       update: guarded, check: "balance >= 0" }  // minor units
  version:    { type: u64,       rowversion: true }
  created_at: { type: timestamp, default: now }
  updated_at: { type: timestamp, on_update: now }
}
table users {
  id:         { type: i64,  pk: true, auto_increment: true }
  first_name: { type: text, update: free }
  data:       { type: json }   // opaque in v1
}
```

### 4.4 DDL (v1)
`create table`, `drop table`, `alter table add column`, `create index`, `drop index`.
(Drop/rename/modify column and other ALTER forms are v2.)

---

## 5. Query model

Queries are **structured data encoded in MessagePack** — *data, never code*. There is no expression
`eval`, no user-defined functions, and any node type outside this grammar is rejected.

### 5.1 Two surfaces, one core
There is a single internal **logical-plan IR** (a normalized operator tree). Two equivalent
**surface forms** lower into it:

- **Pipeline form** — an ordered array of *stages*. Composable and expressive; the foundational
  surface. "HAVING" is simply a `match` stage placed **after** a `group` stage.
- **Clause form** — `from / where / group_by / having / order_by / select / limit`. Ergonomic and
  familiar; **sugar** that compiles into the same stages/IR (clauses are a fixed-order pipeline:
  FROM → WHERE → GROUP → HAVING → PROJECT → ORDER → LIMIT).

Both forms are accepted on the wire and produce identical results. **Stage order is logical**: the
planner is free to reorder safe stages (e.g. push a filter ahead of a group or join) for
performance — it is *not* obliged to execute stages in written order.

### 5.2 Expressions
Column references are **explicit**; any bare scalar is a **literal**.

```
{col: "name"}                  column ref (single table)
{col: ["alias","name"]}        qualified column ref
<scalar>                       literal: int, float, bool, string, null
{eq:[a,b]} {ne:[a,b]} {lt:[a,b]} {lte:[a,b]} {gt:[a,b]} {gte:[a,b]}
{and:[...]} {or:[...]} {not:x}
{add:[a,b]} {sub:[a,b]} {mul:[a,b]} {div:[a,b]} {mod:[a,b]}
{is_null:x} {is_not_null:x}
{between:[x, lo, hi]}
{in:[x, [v1,v2,...]]}
{like:[x, "abc%"]}  {ilike:[x, "abc%"]}     wildcards: % (any run) _ (one char)
{coalesce:[a,b,...]}  {nullif:[a,b]}
{cast:[x, "i64"]}
{sum:x} {count:x} {min:x} {max:x} {avg:x}    aggregate expressions (in group stage)
```

### 5.3 Pipeline form
```
{ op:"select",
  pipeline: [
    { scan: { table:"users", as:"u" } },               // implicit if `from`-style head used
    { match: <expr> },                                  // WHERE
    { join: { type:"inner"|"left"|"cross",
              table:{table:"orders", as:"o"},
              on:<expr> } },
    { group: { by:[ {col:["u","id"]} ],
               agg:{ spent:{sum:{col:["o","amount"]}}, n:{count:1} } } },
    { match: <expr-over-grouped> },                     // HAVING (match after group)
    { sort:  [ { expr:{col:"spent"}, dir:"desc" } ] },  // ORDER BY
    { project: [ {col:["u","name"]}, {col:"spent"} ] }, // SELECT list
    { distinct: true },
    { limit: 20, offset: 0 },
    { cursor: <opaque token> }                          // resume keyset pagination
  ] }
```

### 5.4 Clause form (sugar)
```
{ op:"select",
  from:   { table:"users", as:"u" },
  joins:  [ { type:"inner", table:{table:"orders",as:"o"}, on:<expr> } ],
  where:  <expr>,
  group_by: [ {col:["u","id"]} ],
  having: <expr>,
  order_by: [ { expr:{col:"spent"}, dir:"desc" } ],
  select: [ {col:["u","name"]}, {as:["spent", {sum:{col:["o","amount"]}}]} ],
  distinct: false,
  limit: 20, offset: 0,
  cursor: <opaque token> }
```

### 5.5 DML
```
// INSERT
{ op:"insert", table:"users",
  rows:[ { first_name:"Ada", data:{role:"admin"} } ] }

// UPDATE — guarded relative
{ op:"update", table:"accounts",
  where:{ and:[ {eq:[{col:"id"}, "<uuid>"]}, {gte:[{col:"balance"}, 50]} ] },
  set:{ balance:{ sub:[{col:"balance"}, 50] } } }

// UPDATE — version-guarded (optimistic)
{ op:"update", table:"accounts",
  where:{ and:[ {eq:[{col:"id"}, "<uuid>"]}, {eq:[{col:"version"}, 5]} ] },
  set:{ balance: 30 } }

// UPDATE — free column, explicit unconditional blind set
{ op:"update", table:"users", unconditional:true,
  where:{ eq:[{col:"id"}, 42] }, set:{ first_name:"John" } }

// DELETE
{ op:"delete", table:"sessions", where:{ lt:[{col:"id"}, 1000] } }

// TRANSACTION — atomic multi-op
{ op:"transaction", ops:[ {op:"update", ...}, {op:"update", ...} ] }
```

### 5.6 Results & pagination
```
{ ok:true,
  columns:["name","spent"],
  rows:[ ["Ada", 120], ... ],
  cursor:<token|null>,        // present when more pages remain
  applied:<bool|null>,        // for guarded/conditional writes
  affected:<n|null> }         // rows changed by a write
```
Pagination is **keyset/continuation-token** based (stable under concurrent writes), not numeric
offset. A held cursor pins a read snapshot; results are consistent for the life of the cursor.

### 5.7 EXPLAIN
`{ op:"explain", query:<any select> }` returns the logical plan and the chosen physical plan
(operators, index usage, join order) without executing it.

---

## 6. Safety rules (enforced by the validator — non-negotiable)

1. Every `update`/`delete` **must** carry a row selector (a `where`), or an explicit `{all:true}`.
   A missing selector is a protocol error.
2. A `guarded` column **cannot** receive a blind absolute set. It requires either a relative
   expression or an update carrying a guard/version condition. (A genuine blind set requires
   `unconditional:true` **and** a `free` column.)
3. The read → condition-check → write of a guarded update is a **single atomic step executed in the
   writer against live committed state** — never against a snapshot the client read earlier. This
   eliminates the lost-update/overdraft class.
4. The optimistic path (`expect`/version predicate) detects a concurrent change and fails the
   operation; the client retries. **First-committer-wins.**

---

## 7. Transaction guarantees

- **Atomicity:** a transaction applies fully or not at all.
- **Consistency:** all constraints (PK, NOT NULL, UNIQUE, CHECK, types, guarded-column rules) hold
  at every committed state.
- **Isolation:** readers see a **consistent snapshot**; the single serialized writer makes write
  transactions effectively **serializable**. Readers never block the writer; the writer never
  blocks readers.
- **Durability / no data loss:** a successful commit is **fsync-durable**. After any crash, on
  reopen the database recovers to exactly the last committed transaction, with no torn or partial
  state visible. (Throughput is raised via group commit without weakening this guarantee.)

---

## 8. Security model & guarantees

In scope (the engine must provide):
- **Adversarial-input resistance:** the query decoder enforces maximum nesting depth, node count,
  and message size, and never panics, over-allocates, or stack-overflows on malformed/hostile input.
- **Per-query resource limits:** configurable caps on materialized rows, join count, sort/group
  memory, open cursors, and an optional execution deadline; exceeding a cap is a clean typed error.
- **Arithmetic safety:** all value arithmetic uses checked operations; overflow is a typed error,
  never wraparound or panic (critical for guarded financial-style updates).
- **No code execution:** the AST is strictly data; unknown node types are rejected.
- **Corruption detection:** per-page and per-meta checksums; corrupt pages are surfaced as errors,
  never served as data; recovery never adopts a checksum-invalid meta page.

Explicitly **out of scope for v1** (integrators must not assume these):
- At-rest **encryption**, **authentication**, and **access control** belong to a future host layer,
  not the embedded engine.

---

## 9. Error semantics

Errors are typed and actionable. Categories:
`Validation` (malformed/oversized query, type error, safety-rule violation),
`Constraint` (PK/UNIQUE/CHECK/NOT NULL),
`Conflict` (optimistic version mismatch),
`NotFound`,
`Corruption`,
`ResourceLimit`,
`Io`.
No stringly-typed errors; each carries enough context to act on.

---

## 10. File-format contract (high level)

- A single file of fixed **4 KiB pages**. Pages 0 and 1 are **double-buffered meta pages**.
- Every page carries a checksum. Every meta page records the format version, page size, a monotonic
  committed transaction id, and the root page-ids of the catalog and free-page list.
- **Commit:** new pages are written, fsynced, then the meta page is written to the *inactive* slot
  and fsynced; that slot becomes current.
- **Recovery:** on open, both meta slots are validated by checksum; the valid one with the highest
  transaction id is adopted; unreferenced partial pages are ignored.

Layout details are finalized in `ARCHITECTURE.md`.

---

## 11. v1 scope (authoritative)

| Area | In v1 |
|---|---|
| Statements | insert, select (pipeline or clause), update, delete, transaction (begin/commit/abort) |
| Stages / clauses | scan, match (WHERE), join, group, having (match-after-group), project, distinct, sort (ORDER BY), limit/offset, keyset cursor |
| Joins | INNER, LEFT, CROSS (nested-loop, index-assisted) |
| Operators | `= <> < <= > >=`, AND/OR/NOT, `+ − * / %`, IS [NOT] NULL, BETWEEN, IN(list), LIKE/ILIKE |
| Aggregates | COUNT, SUM, MIN, MAX, AVG (+ group + having) |
| Scalar | CAST (basic), COALESCE, NULLIF |
| Constraints | PK (required), NOT NULL, UNIQUE, CHECK, DEFAULT, foreign keys (RESTRICT; composite/self-referential), auto-increment, generators (now, uuid_v7), rowversion, on_update:now |
| Indexes | B+tree single-column, composite, unique; auto-maintained |
| Tooling | EXPLAIN |
| Types | null, bool, i64, f64, text, blob, uuid, json (opaque), timestamp |

**Deferred to v2+:** foreign-key `CASCADE` / `SET NULL` actions; hash/merge join; RIGHT/FULL join; UNION/INTERSECT/EXCEPT;
subqueries; CTEs; window functions; UPSERT/MERGE; CASE; string/numeric/date functions; JSON path
queries + JSON-path indexes; partial/expression indexes; ALTER beyond add-column; generated columns;
savepoints; views; sequences; collations; decimal/money type; compaction/vacuum; cost-based
optimizer; the network (D1-style) host; authentication/authorization; at-rest encryption;
replication/read-replicas; per-language SDKs.

---

## 12. Glossary

- **CoW B+tree** — copy-on-write B+tree; modifications copy the path to a new root, leaving old
  versions intact for concurrent readers (basis of MVCC + crash safety).
- **MVCC** — multi-version concurrency control; readers see a consistent snapshot without locking.
- **Snapshot** — a pinned root page-id giving a stable read view.
- **Group commit** — batching several write transactions into one fsync.
- **Guarded column** — a column whose updates must be relative or condition/version-checked, never
  a blind absolute set.
- **Logical-plan IR** — the normalized operator tree both query surfaces lower into; the single
  internal representation the validator/planner/executor operate on.
- **Keyset cursor** — pagination by remembering the last key seen (stable under concurrent writes),
  not by numeric offset.
