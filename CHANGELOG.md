# Changelog

All notable changes are recorded here, in [Keep a Changelog][kac] style,
organized under a per-phase heading (see `PLAN.md` §2). Each entry is one line
under a category (`Added` / `Changed` / `Fixed` / `Removed` / `Security`).

[kac]: https://keepachangelog.com/en/1.1.0/

## [Unreleased]

### Phase 11 — Playgrounds, hardening & benchmarking

#### Added
- Acceptance scenarios (`PLAN.md` §7) driven end-to-end through the public
  `otf_dbms` API: indexed lookup verified via EXPLAIN (2), a three-table INNER
  join + GROUP BY matching across both the pipeline and clause surfaces (3), the
  bank scenario under concurrency (5), optimistic version-guard first-committer-
  wins (6), and guard-rule enforcement (7). (1 and 4 already covered in `api.rs`.)
- `otf-dbms`: re-export `CheckExpr` (and `catalog::CmpOp` as `CheckCmpOp`) so
  `CHECK` constraints are declarable through the public API alone; `Response`
  now derives `Debug`.
- `query`: **per-query resource caps** (`ResourceLimits`) enforced by the
  streaming executor — a row cap at every materialization point (bounds sort,
  group, distinct, join inner side, and the final page), a join-count cap, and
  an optional wall-clock deadline; breaching one is a clean `ResourceLimit`
  error. `execute_query`/`execute_page` apply generous defaults;
  `execute_page_with` takes explicit limits (D30).

#### Changed
- CI: added a **loom** job that runs the registry's writer/reader model
  (`RUSTFLAGS=--cfg loom`) on every push/PR, so the concurrency proof can't
  silently rot.
- Docs: recorded that a single write transaction's dirty working set is
  memory-bounded only — dirty pages are pinned until commit, so no spill exists
  in v1; a per-transaction dirty-page cap is the tracked mitigation (D29).

#### Fixed
- `txn`: a **rejected transaction now reclaims the pages it allocated**. A
  multi-op batch that mutates and then fails (violating validate-then-apply)
  used to leak its copy-on-write pages — unpublished but never freed, growing
  the file. The pager records each transaction's allocations and the writer
  returns a rejected job's pages to the free list inside the next committing
  batch (D28).
- `txn`: `WriterStopped` now carries the **category** of the fatal error that
  stopped the writer, so a client sees `Corruption` vs. `Io` faithfully instead
  of everything collapsing to `Io`.
- `otf-dbms`: `Database::inspect` counts rows with an O(1)-memory cursor
  (`CatSnapshot::row_count` → `Snapshot::count_in` → `Cursor::count`) instead of
  materializing every row of every table.

### Phase 10 — Public API & tools

#### Added
- `otf-dbms`: the **public embedded API** — `Database` wraps the engine behind
  a small, hard-to-misuse surface (`ARCHITECTURE.md` §4):
  - **Open/create** — `create`/`open` (file-backed, unix), `create_memory`
    (in-RAM), and `create_with`/`open_with` (injected `Clock`/`Rng` for
    deterministic simulation); `create` rejects a non-empty file.
  - **DDL** — `create_table`/`drop_table`/`add_column`/`create_index`/
    `drop_index` delegating to the catalog (DDL is out of the wire protocol; D27).
  - **Execute** — `execute(&Request)` (validated select/explain/write/txn →
    `Response`), `transaction(ops)`, and `execute_wire(&[u8])` (bytes-in/out).
  - **Cursor** — `open_cursor(select, page_size)` returns a `Cursor` that
    **owns a pinned snapshot** and pages an ordered query with keyset tokens;
    the snapshot is held until the cursor drops, giving cross-page stability
    under a concurrent writer (acceptance scenario 4; D27).
  - **Result decoding** — `Response`/`Row` with by-name access and typed
    accessors (`get_i64`/`get_f64`/`get_bool`/`get_text`) that separate null
    (`Ok(None)`) from an unknown-column/type-mismatch `DecodeError`.
  - **Tools** — `check` (full integrity check: pager invariants + every tree +
    each index cross-checked, surfacing a corrupt page as `Corruption`) and
    `inspect` (structural summary: storage stats + per-table row/index counts).
  - `Error` gains `Io`, `Decode`, and `Usage` variants, each mapped to its
    `SPEC.md` §9 category.
- `cli`: `otf-dbms check|inspect <file>` — read-only file tools over the API.
- Tests: doc-examples on every significant public item (compile + run); an
  open→write→reopen→read file round-trip (scenario 1); an integrity check that
  flags an intentionally corrupted file; and a snapshot-owning cursor paging a
  stable view while a concurrent writer inserts/updates/deletes (scenario 4).

#### Changed
- `dbms` crate: promoted from the Phase 1 error-aggregation scaffold to the
  full public API; `cli` from a placeholder to the file-tools binary.

### Phase 9 — Validator, planner, executor & write path

#### Added
- `query`: the **validator** (`validate` / `validate_select`) — the layer
  between lowering and the planner that makes a well-formed query safe to
  plan and execute against a concrete schema (`ARCHITECTURE.md` §3.8):
  - **Name resolution** over the IR — a `RowType` (the columns visible at
    each operator) threads up the `Plan` tree; column refs resolve
    qualified/unqualified, with typed unknown/ambiguous errors; a select
    returns its `OutputSchema` (labels + kinds + nullability) for `SPEC.md`
    §5.6 `columns` and EXPLAIN.
  - **Expression type-checking** (`SPEC.md` §3/§5.2) — comparisons over
    compatible kinds (equal, or the two numeric kinds; `null` is a wildcard),
    numeric arithmetic with `f64` contagion, boolean predicates, text-only
    `like`, `coalesce`/`nullif` kind unification, `cast`, and `json` treated
    as opaque (rejected from comparison/`between`/`in`/`order by`/`group
    by`/`cast`); LEFT-join right side made nullable; aggregate result kinds
    (count→i64, sum/min/max→arg, avg→f64) with aggregates valid only as named
    group outputs.
  - **`SPEC.md` §6 safety rules** for writes — mandatory row selector
    (rule 1); a `guarded` column may not take a blind absolute set (rule 2),
    admitted only via a relative expression or a conservatively-detected
    guard/version predicate, never via `unconditional:true`; engine-managed
    (`rowversion`/`on_update: now`) and primary-key columns rejected;
    `set`-expression assignability checked against the column.
  - A `SchemaView` schema-source abstraction (implemented for
    `catalog::CatSnapshot`, backed by a map in tests) and the typed,
    `Validation`-category `ValidateError`; `QueryError` gains a `Validate`
    variant.
- Tests: 24 unit cases (in-memory schema) plus a live-`CatSnapshot`
  integration suite (bank-scenario guarded update, blind-set/missing-selector
  rejection, output schema).
- `DECISIONS.md` D24: post-group column visibility, conservative guard
  detection, and the validator ↔ write-path (`Validation` vs `Constraint`)
  boundary.
- `query`: the scalar **expression evaluator** (`eval` / `eval_predicate`) — a
  pure `Expr` → `Value` function shared by the executor and write path. SQL
  three-valued logic (a `null` operand propagates to `null`; only a `true`
  predicate keeps a row), checked `i64` arithmetic (overflow and `/0`,`%0` are
  typed `EvalError`s, `SPEC.md` §8) with IEEE `f64`, value-based mixed numeric
  comparison, `between`/`in`, a `%`/`_` `like`/`ilike` matcher, `coalesce`/
  `nullif`, and casts; a `Shape` binds column references to row positions.
  `DECISIONS.md` D25 records the runtime-error category and the `f64` policy.
- `query`: the **reference executor** (`execute_reference`) — a brute-force,
  fully-materializing interpreter of the logical plan over a `CatSnapshot`:
  base/index scan (an `IndexScan` returns the same rows as scan+filter),
  nested-loop INNER/LEFT/CROSS join, filter, group + aggregates (count/sum/
  min/max/avg, checked `i64` sum, global-aggregate-over-empty), HAVING, sort
  (multi-key, asc/desc, nulls-first), distinct, and limit/offset. It is the
  oracle for the pull-based executor and the first end-to-end read path
  (cursor/keyset pagination lands with that executor). 12 integration tests,
  incl. pipeline↔clause result-equivalence on a join+group+having query.
- `catalog`: **conditional multi-row writes** — `update_where` / `delete_where`
  driven by a caller-supplied `RowUpdater` / `RowFilter` policy that the
  catalog runs against **live committed rows inside the single writer**
  (`SPEC.md` §6 rule 3), the mechanism that serializes guarded read-check-write
  and gives first-committer-wins. Validate-then-apply with full CHECK / index
  maintenance, including in-batch unique-key swaps; a new `CatalogError::Policy`
  preserves the policy's typed error and §9 category across the writer thread.
- `query`: the **write path** (`execute_write`) — inserts map to the catalog's
  atomic `insert_many`; updates/deletes build a policy from the validated AST
  and the scalar evaluator, so relative (`balance - x`), guarded, and
  version-optimistic updates evaluate against live state; returns the `SPEC.md`
  §5.6 `applied`/`affected` outcome. Tests: the **bank scenario** (§7.5) run
  200× under a barrier (exactly one of two concurrent withdrawals commits, no
  overdraft, CHECK always holds), optimistic first-committer-wins, relative
  debit, conditional delete, and guarded-blind-set rejection at validation.
- `query`: the rule-based **planner** (`plan`) and **EXPLAIN** (`explain` /
  `render_plan`, `SPEC.md` §5.7). Two semantics-preserving rewrites of the
  logical plan: **index selection** (a `Filter` of equalities over a `Scan`
  becomes an `IndexScan` on the longest covered secondary-index prefix, with a
  residual filter for the rest — acceptance scenario 2) and **filter
  pushdown** (conjuncts of a `Filter` over an INNER/CROSS `Join` pushed to the
  side they reference, enabling a further index; skipped across LEFT joins to
  preserve null semantics). EXPLAIN renders the physical plan as an indented
  operator tree. 6 tests, incl. the invariant that the **planned** plan
  executes to exactly the reference executor's rows.
- `catalog`: **atomic multi-op transactions** — `write_batch` runs a sequence
  of `WriteSpec`s (insert / conditional update / conditional delete) as one
  writer transaction; each op sees the previous ops' effects and any failure
  rejects the whole batch.
- `query`: the **top-level engine** (`execute_query` / `execute_wire`) — the
  full `ARCHITECTURE.md` §4 journey: validate → (read: lower → plan → execute →
  `columns`/`rows`) / (explain: plan → text tree) / (write: run in the writer →
  `applied`/`affected`) / (transaction: validate all, commit as one atomic
  batch). `execute_wire` is the bytes-in/bytes-out seam that decodes a hardened
  request and encodes a `SPEC.md` §5.6 result — or a typed `{ok:false, code,
  error}` on failure. 8 end-to-end tests, incl. atomic transaction commit and
  rollback-on-duplicate-PK, and the wire round-trip.
- `query`: the **pull-based (streaming) executor** (`execute_stream` /
  `execute_page`) — the streamable operators (scan, filter, project,
  nested-loop join, limit) compose as row iterators that pull on demand (an
  unsorted `Limit` stops early), while the blocking operators (sort, aggregate,
  distinct) buffer and reuse the reference executor's computation. Checked for
  **result-equivalence against the reference executor** over random
  data/queries (the exit criterion). Now the engine's read path.
- `query`: **keyset cursor pagination** (acceptance scenario 4) — `execute_page`
  peels a top-level `Limit`/`Cursor`, resumes past the cursor's sort key, pages,
  and emits a continuation token (the last row's sort key as an `encode_row`
  payload under the tamper-checked envelope) when more rows remain. Within a
  pinned snapshot, paging visits every row exactly once, in order; a mangled
  token is a clean `Validation` error. `DECISIONS.md` D26 records the
  reference/streaming split and the pagination pass (held-snapshot cross-page
  stability is the Phase 10 cursor API).

### Phase 8 — Query protocol, surfaces & IR

#### Added
- `proto`: the hardened wire layer — MessagePack bytes ⇄ a bounded `Doc`
  tree under `DecodeLimits` (max message size checked up front, max depth
  via explicit counter, max node count charged before any allocation);
  rejects the reserved byte `0xC1`, ext types, non-string/duplicate map
  keys, invalid UTF-8, over-`i64` ints, and trailing bytes (`DECISIONS.md`
  D20).
- `proto`: the typed query AST for **both surfaces** — pipeline stages
  (scan/match/join/group/sort/project/distinct/limit/cursor) and the clause
  form (from/joins/where/group_by/having/order_by/select/distinct/limit/
  offset/cursor) — the full §5.2 expression grammar, and DML
  (insert/update/delete/transaction/explain) with faithful selector
  decoding (`where`/`{all:true}`/absent) for the Phase 9 validator.
- `proto`: strict grammar enforcement at decode — unknown ops, stages,
  expression nodes, and fields are typed `Validation` errors (queries are
  data, never code); plus the canonical AST → wire encoding (decode ∘
  encode = identity).
- `proto`: protocol versioning — optional request `v` (missing = 1, other
  values rejected); results always carry `v:1` (`DECISIONS.md` D21).
- `proto`: the logical-plan IR (`Plan`): Scan, IndexScan (planner-only),
  Filter, Join, Aggregate, Project, Distinct, Sort, Limit, Cursor.
- `proto`: result encoding per `SPEC.md` §5.6 (`{v, ok, columns, rows,
  cursor, applied, affected}`), the error-result shape (`{v, ok:false,
  code, error}` with the §9 category as `code`), and the opaque cursor-token
  envelope `[version][crc32c][payload]` with tamper rejection.
- `query`: surface → IR lowering — the pipeline folds directly into a
  `Plan`; the clause form desugars into its fixed-order pipeline and reuses
  the same fold, making clause↔pipeline equivalence true by construction;
  select-list aggregates become named `group` outputs (`DECISIONS.md` D22).
- Exit-criteria tests: decode round-trips for every grammar node; oversized/
  over-deep/over-budget messages rejected; a seeded 200k-input adversarial
  fuzz suite over the decoder (random bytes + corpus mutations + hostile
  container shapes, `DECISIONS.md` D23); clause↔pipeline equivalence on the
  SPEC worked example plus 2 000 random generated pairs.

#### Changed
- `common`: gained the in-house CRC32C routine; `pager` now re-exports it
  (`pager::crc32c` unchanged for callers) so `proto` can checksum cursor
  tokens without depending on storage crates.

### Phase 7 — Indexing

#### Added
- `index`: the secondary-index entry contract — keys are the
  order-preserving encoding of the indexed columns (+ encoded-PK suffix for
  non-unique indexes); entry values are the encoded PK; unique indexes skip
  rows with NULL indexed columns, non-unique include them (`DECISIONS.md`
  D18) — plus probe/prefix-scan bounds and thin maintenance ops over
  `txn::WriteCtx`.
- `catalog`: `IndexDef` (single-column, composite, unique) persisted in the
  table definition (codec v2; v1 records still decode); per-index
  `("iroot", table, index)` root entries committed atomically with the base
  root.
- Index DDL: `create_index` (validates, **backfills from existing rows**,
  rejects unique violations found in the data) and `drop_index` (frees the
  index tree; the implicit backing of a `unique` column cannot be dropped).
- Automatic maintenance in the same write transaction as every insert /
  update / delete — updates touch only indexes whose keys changed; multi-row
  atomicity covers index entries (in-batch unique collisions included).
- `unique` columns now create **implicit unique indexes**
  (`uniq_<table>_<column>`) and are enforced by index probes — D16's
  provisional scan probe is deleted.
- `CatSnapshot::validate()`: structural validation of every tree plus an
  entry-for-entry **brute-force cross-check of every index against its base
  table**, over the snapshot's pinned version.
- Exit-criteria tests: seeded random-DML property test (insert/update/delete
  against a model; every index brute-force-checked along the way), unique
  violations at insert/update/backfill, atomicity, page reclamation for
  dropped indexes/tables, pinned-snapshot consistency.

#### Changed
- `txn`: `Snapshot::validate_tree` exposes the B+tree structural validator
  for trees under the pinned root.

#### Fixed
- `txn`: the writer reclaimed watermark-cleared pages at the start of
  **every** batch, including batches that end up committing nothing (all
  transactions rejected) — leaving the in-memory freelist ahead of the disk
  until the next commit, which `validate()` reported as freelist corruption.
  Reclamation now runs only inside committing batches (`DECISIONS.md` D19).

### Phase 6 — Schema, catalog & constraints

#### Added
- `catalog`: strict typed tables over the transaction layer — the schema
  model (`TableDef`/`ColumnDef`: types, single/composite PK, NOT NULL,
  UNIQUE, CHECK, DEFAULT values + `now`/`uuid_v7` generators, auto-increment,
  rowversion, `on_update: now`, `update: free | guarded`) with definition
  validation, persisted in the in-file system catalog.
- The system catalog **is** the published-root B+tree (`DECISIONS.md` D14):
  per table a schema entry, a data-root entry, and an auto-increment sequence
  entry — one commit covers schema + data atomically and one snapshot pins
  both consistently.
- DDL as ordinary write transactions: `create_table`, `drop_table` (frees the
  whole table tree via deferred reclamation), `add_column` (nullable or
  constant default; old rows padded lazily on read — D17).
- Row DML with full constraint enforcement on insert and update; multi-row
  `insert_many` is atomic (in-batch PK/UNIQUE collisions included); typed
  per-constraint errors mapped to the `SPEC.md` §9 taxonomy.
- Provisional `CheckExpr` (comparisons + boolean combinators, SQL 3VL: NULL
  passes — D15) and provisional scan-based UNIQUE probes until Phase 7's
  unique indexes (D16).
- Engine-managed values under the writer: auto-increment (durable sequence,
  no reuse across reopen), rowversion (1, then +1 per update), `now` /
  `uuid_v7` defaults and `on_update: now` driven by the injected clock/RNG.
- Exit-criteria tests: create/reopen/inspect round-trip, one violation test
  per constraint, guarded persisted/readable, generated values under a
  manual clock, multi-row atomicity, snapshot consistency across DDL+DML.

#### Changed
- `txn`: write transactions generalized to a `WriteJob` trait run against a
  `WriteCtx` (multi-tree edits, typed post-commit outputs); `Db<B>` is now an
  alias for `JobDb<B, OpsJob>` (API unchanged); the writer classifies job
  errors by category (`Io`/`Corruption` fatal, others reject the one
  transaction); new `TxnError::Rejected` carries a higher layer's typed
  error across the writer thread; `Snapshot` gains `root`/`get_in`/
  `range_in`/`scan_in` for reading trees under the pinned root.
- `btree`: `BTree::pages` collects every page of a tree (drop-table
  reclamation).
- `types`: `UuidV7Gen` now owns `Arc<dyn Clock>`/`Arc<dyn Rng>` so generator
  state can span transactions.

### Phase 5 — Types, values & encoding

#### Added
- `types`: the `Value` model covering every v1 type (`SPEC.md` §3), with the
  engine's logical total order (`Eq`/`Ord` follow it: nulls first, IEEE-754
  float total order with one canonical NaN — `DECISIONS.md` D12).
- The **order-preserving key encoding** (`encode_key`/`decode_key`): bytewise
  comparison of encoded keys equals logical comparison, for single and
  composite keys; prefix-free escape-coded text/blob; exactly decodable;
  `json` is not keyable.
- The row encoding (`encode_row`/`decode_row`): compact, self-describing,
  exact round-trip, typed `RowCorrupt` errors on hostile bytes.
- An in-house, hardened minimal MessagePack codec (`DECISIONS.md` D11): the
  wire mapping for values (`encode_value`/schema-directed `decode_value`;
  uuid as canonical string, timestamp as int) and the `json` well-formedness
  gate (`validate_json`: depth-limited, rejects reserved/ext bytes, trailing
  bytes, truncation).
- `UuidV7Gen`: RFC 9562 UUIDv7 over the injected `Clock`/`Rng`, strictly
  monotonic within a run (counter in `rand_a`, timestamp nudge on overflow),
  plus canonical-string format/parse.
- Exit-criteria tests: the order property (`bytewise_cmp(encode(a),encode(b))
  == logical_cmp(a,b)`) over curated edge cases, seeded random values, and
  composites; key/row/wire round-trips; malformed-json rejection; decoder
  no-panic fuzz; UUIDv7 monotonicity under clock stall/backstep.
- Decision D13: no `u64` value type (SPEC §4.3/§3 inconsistency, raised);
  `rowversion` will be `i64`.

#### Changed
- `clippy.toml`: `allow-panic-in-tests = true`, completing the declared
  "test code may use them freely" scoping of operating rule 3.

### Phase 4 — Transactions, MVCC & durability

#### Added
- `txn`: the embedded database handle — `Db` (create/open/clone), atomic
  multi-op `write` (plus `put`/`delete` helpers), and pinned, consistent read
  `Snapshot`s (`get`/`range`/`scan`).
- A single writer thread owning the pager: drains the write queue, coalesces
  waiting transactions into one **group commit** (one fsync pair for the whole
  batch), and publishes the new version on success.
- **Validate-then-apply** atomicity: every op is checked before any mutation,
  so a transaction is applied whole or rejected whole; post-validation I/O
  errors are fatal — the writer fans out `WriterStopped` and stops, leaving the
  database readable but unwritable (`DECISIONS.md` D8).
- `Registry`: reference-counted snapshot versions and the **reclamation
  watermark** — a page superseded by commit `T` is returned to the allocator
  only once no live snapshot older than `T` remains.
- A `loom` model check of the registry handoff proving a pinned reader can
  never observe its pages reclaimed, over every interleaving; gated behind
  `--cfg loom` so it never enters normal builds (`DECISIONS.md` D9).
- Exit-criteria tests: crash-at-every-fsync-boundary durability (acknowledged
  commits always recover; interrupted ones land whole or not at all), a
  long-pinned reader keeping its exact view across heavy churn while
  reclamation defers then catches up, and a seeded deterministic simulation of
  interleaved writes and snapshot open/close matched against a model.

#### Changed
- `pager`: `commit()` no longer holds the state lock across fsyncs — readers
  proceed during the data/meta syncs; safe under the single-writer regime
  (`DECISIONS.md` D10).
- `btree`: entry-size validation extracted as `check_entry` so the `txn` layer
  can pre-validate transactions before mutating.

### Phase 3 — Copy-on-write B+tree

#### Added
- `btree`: the copy-on-write ordered map over the pager — point lookup,
  insert/delete with node split/merge, and forward/backward range scans. A
  mutation copies the touched path to a **new root** and returns the superseded
  pages (`Edit::freed`) for the `txn` layer to reclaim; the tree never frees a
  page, so an earlier root stays a valid snapshot (`DECISIONS.md` D7).
- Variable-length slotted leaf/internal nodes, one per `Data` page, with a
  bounds-checked decoder that rejects hostile bytes as typed `Corruption` errors
  and never panics; byte-based fill with split / merge / rotate, single entries
  capped at half a page (`EntryTooLarge` otherwise) (`DECISIONS.md` D5).
- `Cursor`: a lazy, bounded, forward/backward range iterator that walks a
  root-to-leaf stack — no leaf sibling pointers, so edits stay O(log n) and old
  roots stay readable (`DECISIONS.md` D6).
- `BTree::validate` proving balanced depth, ordering consistent with separators,
  and non-empty non-root nodes.
- Exit-criteria tests: a seeded model-based property test against
  `std::collections::BTreeMap` (insert/delete/lookup/range, `validate` after
  every step, commit + reopen), a snapshot-isolation test (an old root sees no
  later writes), and node-decoder robustness/fuzz tests.

### Phase 2 — Pager (paged, checksummed, atomically-committing storage)

#### Added
- `pager`: fixed 4 KiB page frames with a self-describing 16-byte header
  (CRC32C, page type, self-id) that detects truncation, bit-rot, and misdirected
  writes; a decoder that returns typed `Corruption` errors and never panics on
  hostile input.
- In-house software CRC32C (Castagnoli), zero-dependency (`DECISIONS.md` D3).
- Double-buffered meta pages (slots 0/1) with an atomic commit: dirty pages →
  fsync → new meta to the inactive slot → fsync → promote. Reopen adopts the
  valid meta slot with the highest committed txn id.
- SQLite-style free-list trunk chain (`alloc`/`free`) reusing freed pages before
  extending the file; `validate()` walks meta + free-list proving range, no
  cycles/duplicates, and length agreement.
- Byte-budgeted LRU page cache (dirty pages pinned until commit).
- Exit-criteria tests: a seeded model-based alloc/free/read/write property test
  across commits/reopens (`DECISIONS.md` D4), a multi-trunk reuse test, an
  injected-fault crash test around the meta swap (reopen lands on the last whole
  commit, never a torn one), and decoder/meta-recovery robustness tests.

#### Changed
- `crates/common/src/io.rs`: `FaultInjectingBackend::reset_counters` (target a
  fault at a specific later operation) and an `IoBackend` impl for `Arc<B>` (a
  test can hold a handle to arm faults while the `Pager` owns the backend).

### Phase 1 — Foundations & scaffolding

#### Added
- Cargo workspace with the crate skeletons from `ARCHITECTURE.md` §2: `pager`,
  `btree`, `txn`, `types`, `catalog`, `index`, `proto`, `query`, plus the public
  `otf-dbms` crate and the `cli` binary crate.
- `common` crate (cross-cutting foundations): the `SPEC.md` §9 `ErrorCategory`
  taxonomy and the `CategorizedError` trait (see `DECISIONS.md` D1).
- Per-crate `thiserror` error enums, each implementing `CategorizedError`;
  `otf-dbms::Error` aggregates them and reports the §9 category, verified by a
  nested-propagation test.
- Injectable host services in `common`: `Clock` (`SystemClock`, `ManualClock`),
  `Rng` (`SeededRng`, SplitMix64), and the `IoBackend` trait with three backends
  — `RealFileBackend` (unix), `MemoryBackend`, and `FaultInjectingBackend`.
- Unit tests for the backends/clock/RNG, including the in-memory IO round-trip
  required by the Phase 1 exit criteria; doc-examples on the public API.
- CI workflow running the `PLAN.md` §2 gates (build, test, clippy `-D warnings`,
  `fmt --check`, `cargo doc`, `cargo audit`, `cargo deny`); `deny.toml`.
- `DECISIONS.md` (decisions log) and this `CHANGELOG.md`.

#### Changed
- `ARCHITECTURE.md` §1/§2: added `common` as the 11th crate and renamed the
  public `core` crate to `otf-dbms` (see `DECISIONS.md` D1, D2).

#### Security
- Operating rule 3 enforced mechanically: `clippy::{unwrap_used, expect_used,
  panic}` denied workspace-wide, relaxed in tests via `clippy.toml`.
