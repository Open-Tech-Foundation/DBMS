# PLAN — Embedded Relational Database Engine (v1)

> **What this document is.** The build roadmap and the agent's operating manual: working rules,
> engineering standards, the global testing strategy, security tasks, playgrounds, the ordered
> implementation phases with exit criteria, and the acceptance scenarios. The behavioral contract is
> in `SPEC.md`; the internal design is in `ARCHITECTURE.md`. Read all three before starting; treat
> `SPEC.md` §4–§8 and `ARCHITECTURE.md` as fixed unless a change is explicitly agreed.
>
> Language: **Rust**. Crate names are generic functional placeholders.

---

## 1. Agent operating rules (follow always)

1. **Work phase by phase, in order.** A phase is done only when every exit-criterion test passes.
   Do not skip or reorder phases; do not start a phase before the previous one's criteria are met.
2. **Test-first where practical.** Write the test/property that defines correct behavior before or
   alongside the code. No phase ships without tests.
3. **No `unwrap()`/`expect()`/`panic!` in library paths.** Return typed errors. Panics only for
   genuine invariant violations (bugs), and documented.
4. **Keep `unsafe` minimal and audited.** Every block needs a `// SAFETY:` comment; run Miri on
   crates that contain `unsafe`.
5. **Keep the build green on every commit:** `cargo build`, `cargo test`,
   `cargo clippy -- -D warnings`, `cargo fmt --check` all pass. **Update `CHANGELOG.md` in the same
   commit** (format in §2). Commit per meaningful change and at minimum once per phase; tag
   `phase-N-complete` at each phase boundary.
6. **When a requirement is ambiguous, stop and ask** with a concrete proposal. Record the
   resolution in a short `Decisions` log (a section in this file or a `DECISIONS.md` — your choice,
   but keep it).
7. **Do not silently deviate** from `SPEC.md`/`ARCHITECTURE.md`. If implementation shows a decision
   is wrong, raise it.

---

## 2. Engineering standards

- **CI gates on every push:** build, test, `clippy -D warnings`, `fmt --check`, `cargo doc`
  (no broken intra-doc links), `cargo audit` (dependency CVEs), `cargo deny` (licenses + advisories
  + ban risky/duplicate deps).
- **Errors:** one `thiserror` enum per crate; the public crate exposes a single `Error`/`Result`
  mapping to the `SPEC.md` §9 categories. Never stringly-typed.
- **Dependencies:** minimal and vetted; prefer std + a small well-known set; justify each new dep in
  the decisions log; track `unsafe` in deps with `cargo geiger`.
- **Docs:** rustdoc on every public item, each with a runnable doc-example.
- **Determinism:** the `pager`/`btree`/`txn` layers take an injected clock, RNG, and IO backend so
  they run under deterministic simulation.
- **Invariants:** `debug_assert!`-guarded checks plus a `validate()` that walks structures and proves
  the `ARCHITECTURE.md` §5 invariants, runnable in tests.
- **Progress tracking (`CHANGELOG.md`):** maintained in *Keep a Changelog* style, organized under a
  per-phase heading, and **updated in every commit**. Each entry is one line under a category
  (`Added` / `Changed` / `Fixed` / `Removed` / `Security`) and references the phase; include the
  commit short-hash or tag where useful. This file is the running progress record.
- **Performance hygiene:** measure with criterion before optimizing; never optimize a hot path
  without a benchmark proving the win; avoid needless alloc/copy on read paths.

---

## 3. Global testing strategy (apply the relevant kinds in every phase)

1. **Unit tests** — per module, happy path + every error path.
2. **Property tests** (`proptest`) — encode↔decode round-trips (values, rows, pages, AST);
   B+tree behaves like a `BTreeMap` reference under random op sequences; **the real executor's
   results equal a brute-force reference executor's** over the same data.
3. **Fuzzing** (`cargo-fuzz`) — the AST decoder and page decoder (never panic/OOM/stack-overflow on
   adversarial input) and B+tree op streams.
4. **Crash/durability** — a fault-injecting IO backend stops writes at arbitrary points (between
   page writes, around the meta swap, mid-fsync); after each injected crash, reopen and assert the
   file is valid, recovers to the last committed txn, shows no torn state, and loses no committed
   data.
5. **Concurrency** — `loom` for the writer-queue/snapshot handoff; plus a stress harness of N
   readers + the writer asserting invariants under load.
6. **Deterministic simulation** (Phases 2–4 especially) — seeded RNG controls scheduling, IO timing,
   and faults so any failure reproduces exactly from its seed.
7. **Integration/acceptance** — the §7 scenarios via the public API.
8. **Benchmarks** (`criterion`) — point read, range scan, insert, guarded update, INNER join,
   GROUP BY, commit throughput (with/without group commit).

**Bar:** every public function and every documented error condition has at least one test.

---

## 4. Security implementation tasks (track to completion)

Implement the `SPEC.md` §8 / `ARCHITECTURE.md` §6 mechanisms as concrete, tested work:
- Decoder depth/node-count/size limits, enforced pre-allocation, no unbounded recursion; fuzzed.
- Per-query resource caps (rows, joins, sort/group memory, cursors, deadline) → `ResourceLimit`
  errors.
- Checked arithmetic on all `Value` math; overflow → typed error (explicit test on the guarded
  balance path).
- Reject unknown AST node types.
- Per-page/meta checksums; corruption surfaced, never served; recovery rejects invalid meta.
- Miri over `unsafe` crates; `loom` over the concurrency handoff.
- Document clearly that auth/authz/encryption are out of scope for the engine.

---

## 5. Playgrounds & tooling (build alongside the engine)

1. **CLI REPL (`cli`):** open/create a file; accept queries in a readable JSON-shaped form;
   transparently encode to the MessagePack AST; run and pretty-print results. `\explain`, `\schema`,
   `\timing`. Primary manual playground.
2. **Scenario runner:** execute a scripted query sequence from a file; snapshot results for
   reproducible demos and golden-file regression tests.
3. **Fuzz harness:** seeded, deterministic fuzzers for the AST decoder and page decoder — run in the
   test suite on every push and on an extended budget on a schedule.
4. **Bench harness:** the criterion suite (§3.8) with a compare-to-previous script.
5. **Inspector / integrity-check:** dump file structure (meta, freelist, per-tree page counts, tree
   depth) and run `validate()` on a file.

---

## 6. Implementation phases

Each phase: **Objective → Deliverables → Strategy → Exit criteria.**

### Phase 1 — Foundations & scaffolding
- **Objective:** a clean, gated workspace to build on.
- **Deliverables:** Cargo workspace + the crate skeletons from `ARCHITECTURE.md` §2; shared error
  scaffolding; CI running all §2 gates; the decisions log; injected clock/RNG/IO traits with three
  IO backends (real file, in-memory, fault-injecting).
- **Strategy:** define the IO backend trait now so every lower layer is testable and simulatable
  from day one.
- **Exit:** CI green; crates compile; in-memory IO backend round-trip tested.

### Phase 2 — Pager
- **Objective:** durable, checksummed fixed pages with atomic meta swap.
- **Deliverables:** page format + checksum; pager over the IO backend; LRU cache with byte budget;
  double-buffered meta pages; free-page allocator; `validate()` for freelist + meta.
- **Strategy:** verify checksum on read, recompute on write; meta swap writes the inactive slot,
  fsyncs, then promotes it.
- **Exit:** property test (random alloc/free/read/write keeps freelist + checksums consistent);
  crash test around the meta swap (reopen picks the correct meta, no corruption); page-decoder fuzz
  clean.

### Phase 3 — Copy-on-write B+tree
- **Objective:** the core ordered map with CoW/MVCC semantics.
- **Deliverables:** CoW B+tree over the pager — insert/delete/lookup/forward+backward range; node
  split/merge; root-handle semantics (old roots stay valid for snapshots); `validate()`.
- **Strategy:** writes copy the touched path and return the new root id; install via `txn`. Use a
  provisional byte-comparable key encoding now; finalize in Phase 5.
- **Exit:** model-based property tests vs `BTreeMap`; a reader on an old root sees no later inserts;
  op-stream fuzz; `validate()` after every randomized sequence.

### Phase 4 — Transactions, MVCC & durability
- **Objective:** correct, durable, concurrent transaction semantics.
- **Deliverables:** single-writer MPSC queue; reference-counted read snapshots; commit pipeline
  (write pages → fsync → meta swap); **group commit**; page reclamation tied to live snapshots;
  crash recovery on open; multi-op write transactions.
- **Strategy:** one writer task drains the queue; each commit bumps a monotonic txn id in the meta;
  reclaim a freed page only when no live snapshot needs it. Model the handoff for `loom`.
- **Exit:** `loom` test of queue + snapshot handoff; durability test (committed txns survive crashes
  at every fsync boundary; uncommitted vanish cleanly); deterministic-sim run reproduces any failure
  from a seed; long-reader isolation property holds.

### Phase 5 — Types, values & encoding
- **Objective:** the value model and its order-preserving, round-tripping encodings.
- **Deliverables:** `Value` for all v1 types; **order-preserving key encoding** (incl. composite
  keys, signed ints, total float order, text, timestamp, uuid, null ordering); row encoding;
  MessagePack value mapping; UUIDv7 generator; json well-formedness.
- **Strategy:** the key encoding is the subtle part — test it exhaustively against logical
  comparison.
- **Exit:** property test `bytewise_cmp(encode(a),encode(b)) == logical_cmp(a,b)`; value/row/wire
  round-trips; UUIDv7 monotonic within a run; malformed json rejected.

### Phase 6 — Schema, catalog & constraints
- **Objective:** strict typed tables with constraints, persisted in-file.
- **Deliverables:** catalog model (tables/columns/types/PK/indexes/`update` policy/defaults+
  generators/`rowversion`/`on_update`/checks); system catalog stored in its own B+trees; DDL
  (create/drop table, add column); constraint enforcement hooks (NOT NULL, UNIQUE, CHECK, DEFAULT,
  auto-increment, rowversion + `on_update:now` bump, generators `now`/`uuid_v7`).
- **Strategy:** bootstrap the catalog from reserved trees referenced by the meta page; DDL is an
  ordinary write transaction; validate schema definitions (PK required, no dup columns).
- **Exit:** create/reopen/inspect round-trip; a violation test for each constraint; `update:guarded`
  persisted and readable; auto-increment + rowversion + `now`/`on_update` behave under the writer.

### Phase 7 — Indexing
- **Objective:** secondary indexes that stay correct automatically.
- **Deliverables:** B+tree secondary indexes (single/composite/unique) keyed by the order-preserving
  encoding (+PK suffix for non-unique); maintenance atomic with base-row writes; unique-violation
  detection.
- **Exit:** property test — after random DML, every index matches a brute-force base scan; unique
  violations rejected; `validate()` cross-checks base ↔ index.

### Phase 8 — Query protocol, surfaces & IR
- **Objective:** define and safely decode both query surfaces and lower them to one IR.
- **Deliverables:** AST types for **pipeline + clause** forms; the **logical-plan IR**; hardened
  MessagePack decode (depth/node/size limits, reject unknown nodes); **lowering** of both surfaces
  into identical IR; result-encoding format incl. keyset cursor token; protocol version field.
- **Strategy:** keep IR separate from wire bytes; build the IR + pipeline lowering first, then the
  clause lowerer as a thin translator; prove both lower to the same IR for equivalent inputs.
- **Exit:** decode round-trips for every node; **fuzzer clean** on adversarial input; oversized/
  over-deep messages rejected; clause↔pipeline equivalence tests (same input ⇒ same IR ⇒ same plan).

### Phase 9 — Validator, planner, executor & write path
- **Objective:** run queries correctly and enforce all safety rules.
- **Deliverables:**
  - **Validator:** name resolution, type-checking, and **`SPEC.md` §6 safety-rule enforcement**.
  - **Planner (rule-based):** index seek vs scan; left-deep join order; **safe stage reordering
    (filter pushdown)**; physical operator selection; EXPLAIN output.
  - **Executor (pull-based):** scan, index seek/range, filter, projection, INNER/LEFT/CROSS
    nested-loop (index-assisted) join, aggregation (COUNT/SUM/MIN/MAX/AVG + group + having), sort,
    limit/offset, keyset cursor.
  - **Write path:** insert/update/delete in the writer; guarded read-check-write atomic against live
    state; relative updates; optimistic version (first-committer-wins).
  - **EXPLAIN**.
- **Strategy:** build a brute-force **reference executor** as a test oracle; implement operators as
  composable iterators.
- **Exit:** result-equivalence vs the reference executor across random schemas/data/queries; every
  safety rule has explicit pass/fail tests; the **bank scenario (§7.5)** passes under concurrency;
  EXPLAIN validated; both surfaces produce identical results.

### Phase 10 — Public API & tools
- **Objective:** a clean embedded library plus inspector/integrity tools.
- **Deliverables:** `core` public API (open/create, execute, transaction, cursor open+fetch, close);
  result-decoding helpers; integrity-check command; file inspector.
- **Strategy:** small, misuse-resistant surface; cursors own their snapshot and release on drop;
  doc-example per public item.
- **Exit:** doc-examples compile and run; open→write→reopen→read integration test; integrity-check
  detects an intentionally corrupted file.

### Phase 11 — Playgrounds, hardening & benchmarking
- **Objective:** make the engine demonstrable, stress-proven, and measured.
- **Deliverables:** the REPL, scenario runner, fuzz harness, and criterion benches from §5;
  documented demo scenarios (incl. §7); a README quick-start.
- **Strategy:** wire playgrounds into CI — fuzzers scheduled (extended budget) and run on every push,
  acceptance scenarios on every push, benches on demand with comparison.
- **Exit:** all §7 scenarios pass; fuzzers clean over a sustained session; a baseline bench report
  is committed.

---

## 7. Acceptance scenarios (must all pass for v1)

End-to-end via the public API:

1. **CRUD + reopen** — create/insert/query/update/delete, close, reopen → state exactly as
   committed.
2. **Indexed lookup** — a point/range query uses an index seek (verified via EXPLAIN) and returns
   the same rows a scan would.
3. **Join + group** — an INNER join across three tables with GROUP BY + aggregates matches the
   reference executor; the same query in **both** pipeline and clause form returns identical results.
4. **Keyset pagination stability** — paging a large table with cursors while a concurrent writer
   inserts/updates: no row skipped or duplicated relative to the cursor's snapshot.
5. **Bank scenario (headline concurrency test)** — one account, balance 100; `balance` is `guarded`
   with `check: balance >= 0`. Two concurrent withdrawals (70 and 50) submitted as guarded relative
   updates (`balance = balance − x WHERE balance >= x`). Assert: serialized by the writer, exactly
   **one** succeeds, the other fails insufficient-funds, final balance **30**, CHECK never violated.
   Run thousands of times under the concurrency playground with randomized timing — the invariant
   holds every time.
6. **Optimistic conflict** — two clients read version V and both attempt a version-guarded update;
   exactly one commits, the other fails the version check and behaves correctly on retry.
7. **Guard-rule enforcement** — a blind absolute set to a `guarded` column is rejected; an
   update/delete with no selector and no `{all:true}` is rejected.
8. **Crash durability** — kill at random points during a write workload; every reopen is valid and
   reflects exactly the committed transactions — no loss, no torn state.
9. **Adversarial input** — fuzzers find no panic/OOM/stack-overflow in the AST/page decoders;
   oversized/over-nested queries are rejected cleanly.

---

## 8. Out of scope — future phases (do not build now)

Each is its own later, phased effort; build only when explicitly requested. Keep the v1 seams clean
(`ARCHITECTURE.md` §8) so these slot in without rewrites.

1. **Query power:** hash & merge joins, RIGHT/FULL joins, UNION/INTERSECT/EXCEPT, subqueries, CTEs,
   window functions, UPSERT/MERGE, CASE, string/numeric/date functions, LIKE on non-anchored
   patterns with specialized indexing.
2. **Schema power:** generated columns, ALTER beyond add-column, partial & expression
   indexes, JSON path queries + JSON-path indexes, decimal/money type, savepoints, views, sequences,
   collations. (Foreign keys were pulled forward into v1 — see `SPEC.md` §4.1.)
3. **Storage:** compaction/vacuum; cost-based optimizer.
4. **Deployment:** the network (D1-style) host wrapping `core` behind an RPC front-end using the same
   MessagePack protocol; then authentication/authorization, at-rest encryption, and
   replication/read-replicas.
5. **Ecosystem:** per-language builder SDKs that emit the same AST fluently.
