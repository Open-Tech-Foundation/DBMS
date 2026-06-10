# Changelog

All notable changes are recorded here, in [Keep a Changelog][kac] style,
organized under a per-phase heading (see `PLAN.md` §2). Each entry is one line
under a category (`Added` / `Changed` / `Fixed` / `Removed` / `Security`).

[kac]: https://keepachangelog.com/en/1.1.0/

## [Unreleased]

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
