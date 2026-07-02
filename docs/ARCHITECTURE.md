# ARCHITECTURE — Embedded Relational Database Engine (v1)

> **What this document is.** The internal design: how the engine is structured, the algorithms it
> uses, its invariants, internal security mechanisms, and the rationale behind major decisions.
> The behavioral contract is in `SPEC.md`; the build sequence is in `PLAN.md`.
>
> Crate/module names are generic functional placeholders; rename when a product name is chosen.

---

## 1. Layered overview

Strict bottom-up layering — each layer depends only on layers below it.

```
┌────────────────────────────────────────────────────────────────┐
│  core — public embedded API (open, execute, transaction, cursor) │
├────────────────────────────────────────────────────────────────┤
│  query — surface decode → lower to IR → validate → plan → execute│
│          + write path (insert/update/delete) + safety enforcement│
├────────────────────────────────────────────────────────────────┤
│  proto — AST (pipeline + clause), logical-plan IR, encode/decode │
├──────────────────────────────┬─────────────────────────────────┤
│  catalog — schema, system     │  index — secondary index         │
│  catalog, constraints         │  maintenance                     │
├──────────────────────────────┴─────────────────────────────────┤
│  types — value model, order-preserving key encoding, row encoding│
├────────────────────────────────────────────────────────────────┤
│  txn — single-writer queue, snapshots, commit, group commit,     │
│        page reclamation, crash recovery                          │
├────────────────────────────────────────────────────────────────┤
│  btree — copy-on-write B+tree                                    │
├────────────────────────────────────────────────────────────────┤
│  pager — file I/O, fixed pages, page cache, meta pages, freelist,│
│          per-page checksums                                      │
├────────────────────────────────────────────────────────────────┤
│  common — error taxonomy; injectable clock / RNG / IO backends   │
└────────────────────────────────────────────────────────────────┘
```

**Concurrency model:** exactly **one writer** (an MPSC-fed task) at any instant; **unlimited
concurrent readers**, each holding a snapshot via the CoW tree. The single-writer model is the
backbone — it removes the need for row/page write locks and makes write transactions effectively
serializable.

---

## 2. Crate layout & responsibilities

A Cargo workspace; crate boundaries enforce the layering and keep each test surface small.

| Crate | Responsibility |
|---|---|
| `pager` | Page format + checksums, file I/O over a swappable backend, LRU page cache, double-buffered meta pages, free-page allocator. |
| `btree` | Copy-on-write B+tree: insert/delete/lookup/range; node split/merge; root-handle semantics. |
| `txn` | Writer queue, read snapshots (pinned roots) with reference counting, commit pipeline, group commit, page reclamation, crash recovery. |
| `types` | `Value` model; order-preserving key encoding; row encoding; MessagePack value mapping; UUIDv7 generator; json well-formedness. |
| `catalog` | Schema model; system catalog (stored in-file); DDL; constraint definitions + enforcement hooks; defaults/generators; per-column update policy. |
| `index` | Secondary B+tree index maintenance, kept atomic with base-row writes. |
| `proto` | AST types for both surfaces; the logical-plan IR; hardened MessagePack decode; result encode. |
| `query` | Surface→IR lowering, validator, rule-based planner, pull-based executor operators, write path + safety enforcement, EXPLAIN. |
| `otf-dbms` | The public embedded API tying everything together; cursor lifetime management. (The org-namespaced public crate; was `core` in earlier drafts — renamed to avoid the std `core` collision. Imported in code as `otf_dbms`.) |
| `cli` | REPL + scenario runner + concurrency playground (see `PLAN.md`); ships the `otf-dbms` binary. |
| `common` | **(11th crate, bottom of the stack.)** Cross-cutting foundations shared by every layer: the `SPEC.md` §9 `ErrorCategory` taxonomy + a `CategorizedError` trait each crate implements, and the injectable `Clock` / `Rng` / `IoBackend` host services with real-file, in-memory, and fault-injecting backends. Deliberately tight — domain newtypes (`PageId`, `TxnId`, `Value`, …) stay in their owning crates. See `DECISIONS.md` (D1). |

Supporting trees: `fuzz/` (cargo-fuzz targets), `benches/` (criterion), `tests/` (cross-crate
integration + acceptance).

---

## 3. Component design

### 3.1 Pager
- Fixed **4 KiB** pages. Page header carries a **checksum** (e.g. CRC32C), page type, and flags;
  the rest is payload. Checksum is verified on read and recomputed on write.
- I/O goes through a **swappable backend trait** with three implementations: real file, in-memory,
  and fault-injecting (for crash tests / deterministic simulation).
- **LRU page cache** with a configurable byte budget.
- **Double-buffered meta pages** (page 0 / page 1). A commit writes the new meta to the *inactive*
  slot and fsyncs; that slot becomes current. This makes commit atomic and crash-safe.
- **Free-page allocator**: a free-list of pages no longer referenced by any live snapshot. No
  compaction in v1 — freed pages are reused.

### 3.2 Copy-on-write B+tree
- The ordered map underlying tables and indexes. A modification **copies the touched path to a new
  root**; old nodes remain valid for existing readers. The operation returns the **new root
  page-id**; the `txn` layer installs it on commit.
- Supports point lookup, forward/backward range scan, insert, delete, node split/merge.
- Keys are the order-preserving byte encoding from `types`, so byte comparison equals logical
  comparison (including composite keys and correct null ordering).

### 3.3 Transactions, MVCC & durability (`txn`)
- **Writer:** a single task draining an MPSC queue; applies one write transaction at a time.
- A **write transaction** reads the **latest committed** state (not a snapshot), produces new pages
  via the CoW tree, then commits: write data pages → fsync → meta swap → fsync.
- **Group commit:** the writer batches several queued transactions into one fsync to raise
  throughput while preserving durability.
- A **read transaction** pins the current root (a snapshot) and is reference-counted. A page freed
  by transaction *T* is reclaimable only once no live snapshot with id ≤ *T* remains — this is what
  lets readers run lock-free against a concurrent writer.
- **Crash recovery on open:** validate both meta slots by checksum; adopt the valid one with the
  highest committed txn id; ignore unreferenced partial pages.
- The writer handoff and snapshot lifecycle are modeled so `loom` can exhaustively check them, and
  the whole layer is driveable by an injected clock/RNG/IO for deterministic simulation.

### 3.4 Types & encoding (`types`)
- `Value` covers all v1 types. The subtle, must-be-correct piece is the **order-preserving key
  encoding**: for any two values, `bytewise_cmp(encode(a), encode(b)) == logical_cmp(a, b)`,
  including signed integers, a total order over floats, text, timestamps, uuids, and composite
  tuples, with defined null ordering.
- Row encoding is separate from key encoding. Wire values map to/from MessagePack. `timestamp` is an
  i64 epoch-microseconds value. `json` is stored as its MessagePack bytes, validated well-formed.

### 3.5 Schema & catalog (`catalog`)
- The schema model captures tables, columns, types, PK, indexes, per-column `update` policy,
  defaults/generators, `rowversion`, `on_update`, and checks.
- The **system catalog is itself stored in B+trees** referenced from the meta page; DDL is an
  ordinary write transaction. Constraint enforcement (NOT NULL, UNIQUE via unique index, CHECK,
  DEFAULT application, auto-increment, rowversion/`on_update` bump) hooks into the write path.

### 3.6 Indexing (`index`)
- Secondary B+tree indexes (single-column, composite, unique). An index key is the encoded
  indexed column(s); for non-unique indexes the PK is appended to keep entries distinct.
- Index maintenance happens **in the same write transaction** as the base-row change, so base table
  and indexes are never observed out of sync.

### 3.7 Query protocol & dual-surface lowering (`proto` + `query`)
- Two surface forms (pipeline, clause) decode from MessagePack, then **lower into the single
  logical-plan IR**. Clauses are a fixed-order pipeline, so the clause lowerer is a thin translator
  producing the same stages.
- **Logical-plan IR operators:** `Scan`, `IndexScan/Seek`, `Filter`, `Project`, `Join(type,pred)`,
  `Aggregate(group_keys, aggs)`, `Sort(keys)`, `Limit(n, offset)`, `Cursor`.
- The validator and planner/executor **only ever touch the IR** — there is no second engine for the
  second surface.

### 3.8 Validator → planner → executor (`query`)
- **Validator:** resolves names against the catalog, type-checks expressions, and enforces the
  `SPEC.md` §6 safety rules (mandatory row selector; guarded-column restrictions; guard/version
  handling).
- **Planner (rule-based):** chooses index seek vs scan, picks a left-deep join order, performs
  safe stage reordering (filter pushdown), and selects physical operators. It emits a plan
  inspectable via EXPLAIN.
- **Executor (pull-based iterators):** table scan, index seek/range scan, filter, projection,
  INNER/LEFT/CROSS nested-loop (index-assisted) join, hash/sort aggregation with group/having, sort,
  limit/offset, and the keyset cursor. Operators compose as iterators; results stream.
- **Write path:** insert/update/delete run in the writer. A guarded update reads live state, checks
  its condition, and writes — atomically, within the writer — so the read-check-write cannot be
  split by a client. The optimistic path compares a supplied version against live state and fails on
  mismatch (first-committer-wins).

### 3.9 Public API (`core`)
- Small, hard-to-misuse surface: open/create, execute a query, run a transaction, open and fetch
  from a cursor, close. A cursor owns its read snapshot and releases it on drop. Plus the
  integrity-check and file-inspector tools.

---

## 4. Data & control flow

**A read query's journey:** wire bytes → `proto` decode (hardened) → lower to IR → `query`
validate (catalog + types + safety) → plan (index/join/stage choices) → execute (pull-based
operators over a read snapshot) → encode results (+ cursor token).

**A write's journey:** wire bytes → decode → validate (incl. safety rules) → enqueue to the writer
→ writer applies against live state (base rows + indexes + constraints, atomically) → commit (write
pages → fsync → meta swap → fsync), batched with other queued writes via group commit → encode
result (`applied`/`affected`).

---

## 5. Key invariants (assert in debug builds; prove in `validate()`)

1. **B+tree:** ordering holds; every inserted key is findable; deleted keys are gone; no dangling
   page references.
2. **MVCC:** a page referenced by any live snapshot is never reclaimed or overwritten.
3. **Index consistency:** every secondary index exactly matches a brute-force scan of its base
   table.
4. **Meta:** at least one meta slot is always valid; the adopted meta has the highest committed txn
   id; checksums verify.
5. **Durability:** every committed transaction is recoverable after a crash; no uncommitted state is
   ever visible post-recovery.
6. **Safety:** no guarded column is ever written by a blind absolute set; every applied
   update/delete had a selector or explicit `all`.

---

## 6. Internal security mechanisms

- **Decoder hardening:** `proto` decoding enforces max nesting depth, max node count, and max
  message size *before* allocating, and uses an explicit stack / depth counter (no unbounded
  recursion). Continuously fuzzed.
- **Resource limits:** the streaming executor honors configurable `ResourceLimits` — a cap on the
  rows buffered at any materialization point (which bounds a sort, group, distinct, join inner side,
  and the final page in one place), a cap on the number of joins in a plan, and an optional
  wall-clock deadline. Breaching one is a clean `ResourceLimit` error, never an OOM or a hang. (A
  cap on the number of *concurrently open* cursors belongs to the cursor-owning API layer, not the
  per-query executor.)
- **Checked arithmetic** everywhere a `Value` is computed; overflow → typed error.
- **`unsafe` policy:** minimized; every block carries a `// SAFETY:` rationale; Miri runs over
  `unsafe`-bearing crates; data races are excluded by construction (single writer + Rust ownership),
  with `loom` proving the handoff.
- **Corruption containment:** checksum failure → `Corruption` error; never served as data; recovery
  refuses checksum-invalid meta pages.
- **TOCTOU-free guarded writes:** read-check-write is one atomic step inside the writer against live
  state.

(Authentication, authorization, and at-rest encryption are deliberately **out of scope** for the
embedded engine — a future host layer's concern. Integrators must not assume them.)

---

## 7. Key decisions & rationale

- **From-scratch storage (not built on LMDB/SQLite):** chosen for full control over the file
  format, the query protocol, and the concurrency model. Accepted cost: significantly more
  implementation and correctness effort — mitigated by the testing strategy in `PLAN.md`.
- **Copy-on-write B+tree (vs WAL-journaled B-tree):** gives MVCC snapshots and crash safety by
  construction (atomic meta swap), with no separate write-ahead log to checkpoint. Trade-off: write
  amplification under heavy random writes — acceptable for the read-balanced target workloads, and
  the reason UUID keys default to time-ordered **v7**.
- **B+tree (vs LSM):** predictable read latency and no compaction stalls suit a balanced/read-heavy
  profile. Heavy-write/high-ingest workloads are explicitly out of scope.
- **Single writer + queue (vs multi-writer locking):** removes per-row/page write locks, makes
  write transactions effectively serializable, and makes guarded read-check-write trivially atomic.
  Trade-off: write throughput is capped at one writer — raised via group commit, not parallelism.
- **MessagePack structured AST (vs SQL text):** queries are data — fast cross-language enc/dec, no
  SQL-injection class, and a natural fit for a future remote/host front-end. A dynamic,
  self-describing format (vs schema-bound FlatBuffers/Cap'n Proto) because the query shape is
  dynamic.
- **Dual surface over one IR (clause + pipeline):** clause form gives familiar ergonomics; pipeline
  form gives composability/customization and makes HAVING just "match after group". Because clauses
  are a constrained pipeline, the second surface is cheap (a thin lowerer), and the engine stays
  single (one IR, one validator/planner/executor).
- **`timestamp` as i64 epoch-micros (vs deferring time entirely):** storing time needs nothing
  exotic and `created_at`/`updated_at` are near-universal; only date *functions* are deferred.
- **Money as i64 minor units (vs float/decimal):** avoids floating-point error in financial paths;
  a real `decimal` type is deferred to v2.

---

## 8. Extension points for v2 (keep these seams clean)

- **Planner is replaceable** — a cost-based optimizer can swap in behind the same IR.
- **New IR operators** (hash join, set ops) and **new stages/clauses** slot in without touching the
  surfaces' shared lowering contract.
- **Protocol is versioned** — new node types are additive.
- **A host layer wraps `core`** (it does not reach inside it), so the D1-style network front-end,
  auth, and replication can be added without engine changes.
- **JSON path access / path indexes** extend `types` + `index` without changing opaque-json storage.
