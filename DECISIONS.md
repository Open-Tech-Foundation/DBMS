# Decisions log

Per `PLAN.md` §1 rule 6, every resolution of an ambiguity or deviation from
`SPEC.md` / `ARCHITECTURE.md` is recorded here with its rationale. Newest first.

---

## D33 — Primary-key equality is a base-tree point lookup (`Plan::PkLookup`)

**Phase:** 11 · **Status:** accepted

The new bench suite showed a `WHERE pk = k` lookup costing ~34 ms on a 100k-row
table — a full scan. The planner's `index_select` only considered *secondary*
indexes (`def.indexes`), so a primary-key equality found no candidate and fell
back to `Scan` + `Filter`. But the base tree **is** the primary-key index (keyed
by the encoded PK), so the most common query — fetch by primary key — was the
slowest path.

**Decision:** add a `Plan::PkLookup { table, alias, key }` access path. When a
filter over a base scan pins **every** primary-key column to an equality value,
the planner emits `PkLookup` (preferred over any secondary index — one base-tree
`get`, at most one row) with the leftover conjuncts kept as a residual filter.
Both executors serve it via `CatSnapshot::get`. The IR is internal (not
wire-serialized), so the new node is free of compatibility concerns, and the
"planned == reference" equivalence tests already cover correctness (`PkLookup`
returns the same row as `Scan` + `Filter`). Result: ~19 µs, ~1700× faster.

**Secondary-index seeks followed** (same commit family): `exec::scan` now serves
a planned `Plan::IndexScan` by range-probing the index tree for the prefix's
primary keys (`CatSnapshot::index_candidates`) and fetching those base rows,
rather than scanning the whole table and retaining the prefix. The probe is a
*superset* filter over encoding-equal leading columns, so the exact equality
retain still runs on the (small) candidate set — results and order are identical
to the full scan, proven by the planned==reference equivalence tests.
`point_read/secondary_eq` drops from ~34 ms to ~135 µs (10k rows).

**Still scans:** a partial-PK prefix or a range predicate (`pk > k`, `col > k`).
Extending both trees to range access paths is a further optimization. Separately,
the write path is slow at scale — large batch `insert` and `create_index`
backfill (a 10k unique-index build is ~1.4 s) are the next write-side targets,
independent of these read paths.

---

## D32 — Foreign keys: RESTRICT/CASCADE/SET NULL on both `on_delete` and `on_update`, via a read-only closure planner

**Phase:** v2 schema power (`PLAN.md` §8.2) · **Status:** accepted

Foreign keys were the first v2 "schema power" item pulled forward. The design
splits into a referencing side (child `insert`/`update` must find a parent key)
and a referenced side (parent `delete`/`update`, `drop table`). Both are enforced
in the writer (`catalog::job`) under the existing validate-then-apply contract,
so a rejected write is a guaranteed no-op. Parent existence is probed against the
parent's PK tree or the referenced `UNIQUE` index (both O(log n)); referenced
columns are therefore required at DDL time to be the parent's PK or a unique
index. `MATCH SIMPLE` (a NULL in any referencing column skips the check) matches
the row encoding's existing NULL-in-unique-index handling.

**Decision:** both `on_delete` and `on_update` support **RESTRICT**, **CASCADE**,
and **SET NULL**. To keep the no-op-on-reject guarantee across a multi-table,
possibly cyclic closure, the referenced side runs in three read-only phases
before any mutation: (1) `plan_cascade` builds the full closure of doomed rows
(CASCADE delete, recursive) and rewritten rows (SET NULL, or CASCADE rewrite to a
changed parent key), following only CASCADE/SET NULL edges; (2)
`check_cascade_restrict` rejects if any *surviving* child still references a
disappearing key through a RESTRICT edge; (3) `validate_cascade_rewrites`
re-checks every rewritten row against its own CHECK / NOT NULL / UNIQUE (across
the whole closure). Only then does `apply_cascade` delete and rewrite with full
secondary-index upkeep. A delete seeds the closure with delete-effects
(`on_delete`); an update that changes a referenced key seeds it with key-change
effects (`on_update`), while the parent row itself is written by the normal
update path. Termination holds: each row is recorded at most once.

**One `on_update` restriction stays** (rejected at DDL): an `on_update` CASCADE
whose referencing columns are part of the *child's* primary key would have to
move the child's key, and v1 keeps primary keys immutable (`PkImmutable`). This
is niche — `on_update` fires only when a referenced *updatable* `UNIQUE` non-PK
key changes, and such a key is rarely also part of the child PK — so it is
rejected rather than supported via a delete+reinsert re-key.

**Alternatives rejected:** (a) applying the closure eagerly row-by-row, which
would break the no-op guarantee when a downstream RESTRICT or a rewrite-violates-
CHECK/UNIQUE is hit mid-closure; (b) requiring no DDL constraint on the parent
columns, which would make every child existence probe O(n) instead of O(log n).

**Referenced-side lookup** (`scan_children`) uses an **index range probe** when
the child has an index whose leading columns are the referencing columns (unique,
non-unique, or a composite index the FK columns prefix) — O(log n) instead of a
full child scan — and falls back to a scan when no such index exists. So a
performance-sensitive `on_delete`/`on_update` FK is served by simply indexing the
referencing columns; auto-creating that index is a possible future convenience.
(The engine is pre-release, so the catalog record format still evolves in place
behind its single version byte — no legacy-decode paths.)

---

## D31 — The free-list is rebuilt into fresh pages each commit (crash-safe), not mutated in place

**Phase:** 11 · **Status:** accepted

Acceptance scenario 8 (crash durability) exposed a real bug: the CoW + meta-swap
design guarantees every page reachable from the *last committed* meta stays
byte-for-byte intact until the *new* meta is durable — which holds for data
pages (new pages go to free slots; old pages are never touched) but **not for
the free-list**. The allocator mutated trunk pages in place and could repurpose
an emptied trunk as a data page, flushing those writes in commit step 2 (data
pages), before the meta swap in step 3. A crash in between recovers the old
meta, whose free-list chain now points at overwritten pages → a corrupt
free-list, and a next allocation that could hand out a live page. (Reads were
always safe — the data tree is intact — but `validate` and the next write were
not.)

**Decision:** the authoritative free set lives **in memory** (`State.free_set`);
`alloc`/`free` only touch it. At each `commit` the set is **serialized into a
fresh trunk chain** whose container pages are drawn from the free set itself —
free-page content the committed meta never reads, so overwriting it is
crash-safe — while the previous commit's trunk pages are left untouched on disk
and recycled into the free set only *after* the new meta is durable. The whole
committed state (data + free-list) is therefore CoW, and recovery to either meta
is consistent. The set is loaded from the durable chain lazily on the first
write, so a read-only open still touches only the meta and a database with a
corrupt free-list stays readable ([[D8]] durability, [[D28]] rejection reclaim).

**Tradeoffs:** (1) a commit rewrites all trunk pages, so a workload with a very
large stable free set pays O(free/`CAPACITY`) trunk writes per commit — a future
incremental-CoW free-list could avoid this (measure first, PLAN §2). (2) A few
freed pages are briefly parked as trunk structure and recycled one commit later,
so a huge free-then-realloc burst without an intervening commit can grow the
file by that bounded trunk overhead; it converges and never leaks.

---

## D30 — Per-query resource caps are enforced at the streaming executor's materialization points

**Phase:** 11 · **Status:** accepted

`SPEC.md` §8 / `ARCHITECTURE.md` §6 / PLAN §4 call for per-query resource caps
(rows, joins, sort/group memory, deadline → `ResourceLimit`), but only the wire
decoder (`DecodeLimits`) was capped; the executor had no enforcement, so a cross
join or an unbounded sort could buffer arbitrarily much memory.

**Decision:** add a `ResourceLimits { max_rows, max_joins, deadline }` and a live
`Budget` threaded through the pull-based executor. The key observation is that
**every buffering step funnels through one `collect`** — the blocking operators
(sort, aggregate, distinct) drain their input there, the nested-loop join buffers
its inner side there, and `execute_page` materializes the final page there — so a
single row cap enforced in `collect` bounds "materialized rows" *and*
"sort/group memory" *and* a cross join's output at once. Join count is a cheap
pre-execution plan walk; the deadline is polled in the same `collect` loop
(against a monotonic `Instant`, not the injectable value-`Clock`, since it guards
execution time, not row contents). The **reference executor stays uncapped** — it
is the correctness oracle for equivalence tests, not a production path.
`execute_query`/`execute_page` apply generous defaults (10M rows, 16 joins, no
deadline); `execute_page_with` takes explicit limits. A cap on *concurrently
open* cursors is a property of the cursor-owning API layer ([[D27]]), not the
per-query executor, and is out of scope here.

---

## D29 — A write transaction's working set is memory-bounded (no spill in v1)

**Phase:** 11 · **Status:** accepted

The page cache pins dirty frames until the next commit (`cache.rs`): they cannot
be evicted, because evicting an uncommitted page would lose the only copy of a
pending write. This is correct, but it means a *single* write transaction's
dirty working set can exceed the cache's byte budget without limit — one huge
transaction (e.g. a multi-million-row insert in one batch) holds every page it
touches in memory until commit.

**Decision:** accept this for v1 as a known, documented bound rather than add a
spill-to-disk path. Rationale: the embedded workloads targeted here commit in
modest batches, and a redo/undo spill log is a substantial subsystem that would
duplicate durability machinery the CoW + meta-swap design deliberately avoids.
The mitigation is an **eventual per-transaction dirty-page cap** — the write-side
counterpart to the read-side resource caps ([[per-query-resource-caps]], PLAN
§4), rejecting an over-large transaction with a `ResourceLimit` error instead of
letting it consume unbounded memory. Until that lands, callers bound their own
batch sizes. Tracked as a Phase-11+ item; recorded here so the "LRU page cache
with a configurable byte budget" description is not mistaken for a hard cap on a
transaction's footprint.

---

## D28 — A rejected transaction reclaims the pages it allocated

**Phase:** 11 · **Status:** accepted

D8 makes single ops validate-then-apply, so a rejection is a no-op that touches
no pages. But the atomic multi-op batch (`catalog::batch`) runs its ops in
sequence: if op 3 fails validation, ops 1–2 have already mutated the CoW tree.
The writer restores the published root (atomicity holds — nothing op 1–2 wrote
is reachable or durable), but the fresh pages ops 1–2 allocated were neither
published nor reclaimed. With no compaction in v1, a workload of recurring
failed batches would grow the file: `meta.page_count` counts those pages, but
nothing ever returns them to the free list.

**Decision:** the pager records the pages each transaction allocates
(`begin_alloc_recording` / `take_alloc_recording`), and on rejection the writer
parks them in an `orphaned` list. Because they are unpublished, they are safe to
free immediately — but, like superseded pages ([[D7]]) and reclaimed pages, they
are freed **inside a committing batch** so the free-list change is made durable
rather than left ahead of the disk (the same invariant that moved `reclaim` into
the commit branch). A rejected transaction is therefore a true no-op that
reclaims its own scratch space. Validate-then-apply is still preferred, since it
avoids the allocate-then-free churn; this only bounds the cost when a job cannot.

---

## D27 — Public API shape: a cursor owns its snapshot; DDL sits on `Database`

**Phase:** 10 · **Status:** accepted

`PLAN.md` Phase 10 asks for a "small, misuse-resistant" embedded API where
"cursors own their snapshot and release on drop". Three shaping calls.

- **Cursor cross-page stability = holding one `CatSnapshot`.** A `Cursor`
  captures `Catalog::snapshot()` at open and pages over it for its whole life;
  the pinned snapshot holds its version live against reclamation (the txn
  registry), so a concurrent writer's inserts/updates/deletes never perturb the
  walk — this is exactly acceptance scenario 4, which Phase 9 deferred to this
  layer ([[phase9-status]], D26). Each `fetch` rebuilds `Limit { Cursor { base } }`
  around the once-planned physical plan and calls the existing `execute_page`;
  no new pagination engine. The paged select must **end in a sort** (keyset
  resume is by the trailing sort key), documented on `open_cursor`. Release is
  automatic: dropping the cursor drops the snapshot.

- **DDL lives on `Database`, not in the request protocol.** `proto::Request`
  is DML + query + transaction only (no `create table`), matching the wire
  protocol. But an embedded library must create schema, so `Database` exposes
  typed DDL (`create_table`/`drop_table`/`add_column`/`create_index`/
  `drop_index`) delegating straight to the catalog, alongside `execute`
  (wire-shaped requests) and `execute_wire` (bytes-in/out). This keeps the
  transport surface minimal while the in-process surface is complete.

- **Result decoding separates null from mistake.** `Row`'s typed accessors
  return `Result<Option<T>, DecodeError>`: `Ok(None)` is a genuine SQL null,
  while an unknown column or a type mismatch is a typed `Validation`-category
  error rather than a silently-swallowed `None`. Reading a result wrong is a
  caller bug and should surface as one.

The file tools (`Database::check`, `Database::inspect`, and the `otf-dbms
check|inspect <file>` CLI) run entirely over the existing pager + snapshot
validators; `check` reads every live page, so a corrupted page trips its
checksum and surfaces as `Corruption`.

---

## D26 — Streaming executor reuses blocking ops; keyset pagination is a top pass

**Phase:** 9 · **Status:** accepted

`PLAN.md` Phase 9 wants a pull-based executor checked against the brute-force
reference executor, plus keyset cursor pagination (§7.4). Two edges to fix.

- **What "pull-based" duplicates.** The streaming executor implements the
  *streamable* operators (scan, filter, project, nested-loop join, limit) as
  composable row iterators — the pull model, where a `Limit` over an unsorted
  scan stops early. The inherently **blocking** operators (sort, aggregate,
  distinct) buffer their input and **reuse the reference executor's tested
  computation** rather than a second copy — exactly the split a Volcano engine
  makes, and it keeps the group/sort/agg semantics (`DECISIONS.md` D24)
  single-sourced. The equivalence test (`streaming == reference`) therefore
  validates the streaming *plumbing*, which is where streaming-only bugs live.

- **Pagination is a top-level pass, not an operator.** Lowering nests the page
  stages as `Cursor{ Limit{ Sort{…} } }` (matching the SPEC §5.3 example), but
  a keyset seek must *resume then page*. Rather than force a planner reorder,
  `execute_page` peels **both** `Limit` and `Cursor` off the plan top (either
  nesting means the same thing) and applies: run the ordered core → drop rows
  up to the resume key → offset/limit → emit a continuation token from the last
  row's sort key when more remain. The token is the sort-key values as an
  `encode_row` payload under the existing tamper-checked envelope
  (`proto::encode_cursor_token`). Keyset resume assumes the **trailing sort key
  is unique** (else equal-key boundary rows can skip/dup — the standard keyset
  caveat; append a unique column). Within one pinned snapshot no row is skipped
  or duplicated; holding a snapshot **across** pages for full §7.4 stability is
  the cursor-owns-its-snapshot public API of Phase 10 (`ARCHITECTURE.md` §3.9).

## D25 — Runtime evaluation errors map to `Validation`; `f64` follows IEEE

**Phase:** 9 · **Status:** accepted

`SPEC.md` §8 requires checked arithmetic ("overflow is a typed error, never
wraparound or panic") but §9's category list has no arithmetic/runtime bucket.
The scalar evaluator's failures — integer overflow, integer division/modulo by
zero, and out-of-range/unrepresentable casts — are data-dependent runtime
faults of an otherwise-valid, validated query.

**Decision:**

- These map to **`Validation`**: the closest fit (the operation is invalid for
  this row), and it keeps the §9 taxonomy fixed rather than inventing a
  category. `EvalError` is a single crate-local enum, so a future v1.x can
  reclassify in one place if a dedicated category is added.
- **Integer** arithmetic is checked (`checked_add`/`sub`/`mul`/`div`/`rem`);
  overflow and `/0`,`%0` are typed errors. **`f64`** arithmetic follows
  IEEE-754: division by zero yields `±inf` and overflow saturates to `inf`
  rather than erroring — matching the value model, which already admits inf/NaN
  as ordinary `f64` values (`DECISIONS.md` D12). Only integer money-style math
  is guarded, which is the §8 concern.
- Mixed `i64`/`f64` **comparison** and **arithmetic** coerce the integer to
  `f64` (the validator permits the mix); comparison is by numeric value, not by
  the variant-rank order `Value` uses cross-type.

## D24 — Post-group visibility, guard detection, and the validator's boundary

**Phase:** 9 · **Status:** accepted

`SPEC.md` §5/§6 fix the validator's job but leave three edges open; the Phase 9
validator resolves them as follows.

- **Base columns stay visible after a `group`.** The `SPEC.md` §5.3 worked
  example projects `{col:["u","name"]}` *after* grouping only by `u.id` — a
  column that is neither a group key nor an aggregate. Standard SQL rejects
  that; this engine (like SQLite/MySQL's extended `GROUP BY`) allows it. So an
  `Aggregate` node's output row type is **the input columns (pass-through) plus
  the named aggregate outputs**, not just the keys. Raw aggregate expressions
  (`{sum:x}`) remain valid *only* as named group outputs; anywhere else (a
  later `match`, `project`, `sort`, or join `on`) they are an
  `AggregateNotAllowed` validation error — reference the named output instead.
  This matches the clause lowerer (D22), which already rewrites select-list
  aggregates into named outputs.

- **Guard detection for §6 rule 2 is conservative.** An absolute set to a
  `guarded` column is admitted when the `where` carries a "guard/version
  condition": a comparison on a `guarded` or `rowversion` column. The scan
  credits such predicates under a top-level `and` and inside `cmp/between/in`,
  but **not** under `or`/`not`, where a guard could be weakened away. A false
  negative only over-rejects (the caller makes the update relative or
  version-guarded); it never lets a genuine blind set through. `unconditional:
  true` never rescues a guarded column — that flag is for `free` columns only.

- **Validator vs. write path.** The validator owns everything
  `Validation`-category: names, expression types, and the §6 safety rules
  (including engine-managed / PK-immutable writes, which `SPEC.md` §9 and the
  catalog both class as `Validation`). It does **not** re-check live
  `Constraint`-category rules — NOT NULL, UNIQUE, CHECK, required-column
  presence, and insert value typing stay in the write path against committed
  data, so the two layers never duplicate (and cannot diverge on) enforcement.

## D23 — Phase 8 fuzzing is a seeded in-house harness; libFuzzer waits for Phase 11

**Phase:** 8 · **Status:** accepted

`PLAN.md` Phase 8's exit demands "fuzzer clean on adversarial input", while
the dedicated fuzz harness (cargo-fuzz/libFuzzer) is a Phase 11 deliverable.
Pulling `cargo-fuzz` forward would add a nightly-only toolchain and external
deps that D4's reasoning avoids.

**Decision:** Phase 8 ships a deterministic, seeded adversarial-input suite
(`proto/tests/proto_fuzz.rs`): 100k random byte strings, 100k corpus
mutations (bit flips, truncations, overwrites, splices), and hand-built
hostile container shapes (depth bombs, claimed-giant containers,
str32/bin32 length lies). Every decoded request must also survive a
canonical re-encode/re-decode round-trip. Coverage-guided fuzzing arrives
with Phase 11's harness; this suite stays as the fast deterministic gate.

## D22 — Clause desugaring semantics: aggregates, distinct, having

**Phase:** 8 · **Status:** accepted

`SPEC.md` §5.4 fixes the clause order (FROM → WHERE → GROUP → HAVING →
PROJECT → ORDER → LIMIT) but leaves three details open. The clause lowerer
desugars into a pipeline and reuses the pipeline fold, so equivalence is by
construction; these rules define the desugaring:

- **Aggregates in the clause `select` list** become named `group` outputs:
  the alias names the output (`{as:["spent",{sum:…}]}` → agg `spent`), an
  unaliased aggregate gets its function name (`{count:1}` → `count`), and a
  name collision is a typed error. v1 allows aggregates only as the *whole*
  select item (no `{add:[{sum:x},1]}`); grouping is implied by `group_by`
  *or* select-list aggregates.
- **`distinct` is an IR operator** (`Plan::Distinct`), placed after PROJECT
  and before ORDER in the clause order. `ARCHITECTURE.md` §3.7's operator
  list omits it though the stage exists in `SPEC.md` §5.3/§11; an explicit
  operator beats desugaring into `Aggregate`, which would entangle lowering
  with planning. (Addition, not contradiction — flagged per rule 6.)
- **`having` without grouping** is rejected at lowering; `{distinct:false}`
  is an explicit no-op stage. Structural shape (pipeline starts at a `scan`,
  later sources arrive via `join`, `group` outputs really are aggregate
  calls) is also enforced at lowering — names/types/§6 safety stay in the
  Phase 9 validator.

## D21 — Protocol version field and cursor-token envelope

**Phase:** 8 · **Status:** accepted

`PLAN.md` Phase 8 requires a protocol version field and a keyset cursor
token; `SPEC.md` §5's grammar shows neither a version nor the token's bytes.

**Decision:**
- **Version:** requests may carry a top-level `v` (int). Missing means
  version 1; any other value is a typed `UnsupportedVersion` error. Results
  always carry `v:1`. Error results are `{v, ok:false, code, error}` with
  `code` the stable `SPEC.md` §9 category identifier (the shape §5.6 leaves
  implicit for the failure case).
- **Cursor token:** opaque bytes `[version 0x01][crc32c(payload) BE][payload]`.
  The payload (keyset position) is defined with the executor in Phase 9; the
  envelope is fixed now so a truncated or mangled token is a clean
  `Validation` error instead of a nonsense seek. CRC32C moved from `pager`
  to `common` (same in-house routine, D3) so `proto` shares it without
  depending on storage crates.

## D20 — Two-stage hardened decode: bytes → bounded Doc tree → AST

**Phase:** 8 · **Status:** accepted

`ARCHITECTURE.md` §6 requires limits enforced *before* allocating and no
unbounded recursion, but does not fix the decoder's architecture.

**Decision:** decoding is two stages. A small hardened reader produces a
generic `Doc` tree (null/bool/int/float/str/bin/array/map) under
`DecodeLimits` — max message size (checked before reading), max depth
(explicit counter), max node count (budget charged per node; container item
counts are validated against both the remaining bytes and the remaining
budget before any `Vec` allocation). It rejects the reserved byte `0xC1`,
ext types, non-string and duplicate map keys, invalid UTF-8, ints outside
`i64`, and trailing bytes. The AST mapping then works on the already-safe
tree and enforces the grammar (unknown ops/stages/expressions/fields are
typed errors). Defaults: 1 MiB / depth 64 (matching `types::MAX_JSON_DEPTH`)
/ 100k nodes, embedder-configurable per `SPEC.md` §8. The node cap bounds
the intermediate tree's memory, so the two-stage shape costs nothing
adversarially and keeps all byte-level hardening in one ~200-line module.
Insert-row values that are containers become `json` values (re-encoded
canonical MessagePack), matching §5.5's `data:{role:"admin"}` example.

## D19 — Reclamation only runs inside a committing batch

**Phase:** 7 · **Status:** accepted (bug fix of Phase 4 behavior)

The Phase 4 writer reclaimed watermark-cleared pages at the **start of every
batch**. A batch whose transactions are all rejected never commits — but
reclamation had already mutated the pager's in-memory freelist, leaving it
ahead of the disk (surfaced by `validate()` as freelist corruption; on a
crash in that window the parked pages would simply leak, as they always do
when the writer's in-memory park list dies — no disk state was ever wrong).

**Decision:** the writer reclaims **after applying a batch's jobs and only
when the batch will commit**, so the freelist changes ride the same fsync
pair. Pages a batch frees become reusable one batch later (instead of within
the same batch) — a negligible cost. Found by Phase 7's `drop index` tests;
regression-tested at the txn layer.

## D18 — Index entry contract: PK as value, NULL rows skip unique indexes

**Phase:** 7 · **Status:** accepted

`ARCHITECTURE.md` §3.6 fixes the key shape (encoded indexed columns, PK
suffix when non-unique) but leaves the entry value and NULL semantics open.

**Decision:**
- **Entry value = the encoded PK** (the base-tree key) for both unique and
  non-unique indexes: uniform, and resolving an entry to its row is one base
  lookup without re-deriving key suffixes.
- **Unique indexes skip rows with any NULL indexed column** — NULLs never
  conflict (D17), and since unique entries carry no PK suffix, storing NULL
  rows would falsely collide. Non-unique indexes include NULL rows (nulls
  sort first, queryable).
- **`unique` columns are enforced by an implicit single-column unique
  index** named `uniq_<table>_<column>` (replacing D16's provisional scan
  probe). The implicit backing cannot be dropped; `drop table` frees index
  trees with the base.
- Index names are per-table; the catalog stores one `("iroot", table, index)`
  root entry per index, updated in the same commit as the base root. The
  table-definition codec is now **version 2** (v1 records still decode,
  index-less).

## D17 — Write-path semantics the SPEC leaves open

**Phase:** 6 · **Status:** accepted

`SPEC.md` §4 defines the constraint set but not every edge of the write path.
Phase 6 fixes these (each is a one-line change later if v2 decides otherwise):

- **PK columns are immutable** — an update touching one is rejected
  (`PkImmutable`). Change-of-key is delete + insert.
- **Engine-managed columns reject explicit writes**: `rowversion` and
  `on_update: now` columns can never be set by the caller.
- **`rowversion` starts at 1** on insert and bumps by 1 on every update.
- **`on_update: now` also stamps on insert** (an `updated_at` is never NULL).
- **Explicit values on auto-increment columns are allowed**; the sequence
  advances past them (`seq = max(seq, given + 1)`), so generated keys never
  collide. The sequence is a catalog entry — durable, gaps allowed, no reuse
  after crash/reopen.
- **`add column`** requires nullable or a *constant* default; existing rows
  are padded lazily on read (generators cannot backfill), no rewrite.
- **UNIQUE ignores NULLs** (multiple NULLs allowed, SQL semantics).

## D16 — UNIQUE enforced by a scan probe until Phase 7

**Phase:** 6 · **Status:** accepted (provisional)

`SPEC.md` §4.1 says UNIQUE is "enforced via a unique index", but `index` is
Phase 7. Phase 6 enforces the constraint **correctly but provisionally** with
a full-table scan probe (one scan per write batch). Phase 7's unique indexes
replace the probe with an index lookup; behavior is unchanged, only cost.

## D15 — Provisional CHECK expressions with SQL three-valued logic

**Phase:** 6 · **Status:** accepted (provisional)

`SPEC.md` §4.1 allows `CHECK(<expr>)` over the row, but the expression
language (§5.2) arrives with the query layer in Phases 8–9.

**Decision:** Phase 6 ships a minimal `CheckExpr` — column-vs-literal
comparisons, `and`/`or`/`not`, `is_null`/`is_not_null` — validated at DDL
(columns exist, literal kinds match) and stored in the catalog. Evaluation
follows SQL 3VL: a CHECK is violated only when it is definitively **false**;
NULL/unknown passes. The Phase 8/9 expression engine supersedes the enum
(same precedent as Phase 3's provisional byte keys).

## D14 — One published root: the catalog tree owns everything

**Phase:** 6 · **Status:** accepted

`ARCHITECTURE.md` §3.5 stores the system catalog "in B+trees referenced from
the meta page" but leaves the multi-tree commit mechanics open.

**Decision:**
- The meta page's root **is the catalog B+tree**; every other tree hangs off
  it. Per table: `("tbl", name)` → schema (changes on DDL), `("root", name)` →
  data-tree root (changes on every write), `("seq", name)` → auto-increment
  cursor. A write to table T updates T's tree, then T's root entry, producing
  one new catalog root — so one root install + one fsync pair commits schema
  and data atomically, and a snapshot pins both consistently.
- `txn` generalizes to **`WriteJob`/`WriteCtx`**: a job runs on the writer
  thread, edits any tree under the root, and returns a typed output delivered
  after durable commit. `Db<B>` is now an alias for `JobDb<B, OpsJob>` — the
  Phase 4 API and tests are unchanged. Jobs inherit D8's validate-then-apply
  contract; the writer classifies job errors **by category** (`Io`/
  `Corruption` fatal, everything else rejects just that transaction, with the
  root and freed-list restored defensively). Rejections cross the thread as
  `TxnError::Rejected(Box<dyn CategorizedError>)` and downcast back to the
  catalog's typed error at the API surface.

## D13 — No `u64` value type; `rowversion` columns are `i64`

**Phase:** 5 · **Status:** accepted (SPEC inconsistency, raised)

`SPEC.md` §4.3's example schema declares `version: { type: u64, rowversion:
true }`, but §3's authoritative type table has no `u64`.

**Decision:** the `Value` model implements exactly the §3 table; there is no
`u64`. A `rowversion` counter fits comfortably in `i64` (~9.2 × 10¹⁸ writes),
so Phase 6 models `rowversion` columns as `i64`. Recorded here rather than
silently extending the type system.

## D12 — Key-encoding order: nulls first, one canonical NaN above +inf

**Phase:** 5 · **Status:** accepted

`ARCHITECTURE.md` §3.4 mandates `bytewise_cmp(encode(a), encode(b)) ==
logical_cmp(a, b)` with "a total order over floats" and "defined null
ordering", but fixes neither choice.

**Decision:**
- **Nulls sort first**, engine-wide (tag `0x01`, the lowest).
- **Floats** use the IEEE-754 total order (`-0.0 < +0.0`), with **every NaN
  canonicalized to the one positive quiet NaN**, sorting above `+inf`. Key
  equality follows the same total order (`NaN == NaN`), so NaN keys are
  well-behaved rather than unmatchable.
- **Variable-length components** (`text`/`blob`) are escape-coded
  (`0x00` → `0x00 0xFF`, terminator `0x00 0x00`): prefix-free, so composite
  keys are plain concatenation and prefixes sort first.
- The encoding is **decodable** (exact round-trip): Phase 7 recovers PK
  suffixes from non-unique index entries instead of storing the PK twice.
- `json` is opaque in v1 and **not keyable** (typed `NotKeyable` error).

## D11 — In-house minimal MessagePack codec

**Phase:** 5 · **Status:** accepted

The wire mapping, `json` storage, and Phase 8's AST decode all need
MessagePack. `rmp`/`rmpv` would be the first unvetted external dependencies
(D4 keeps the graph at `thiserror` only), and the engine needs a *hardened*
decoder (depth limits, no panics on hostile bytes) more than a featureful one.

**Decision:** implement the needed subset in `types::msgpack`: compact int /
str / bin / array / map encode, schema-directed value decode, and a
well-formedness walk with `MAX_JSON_DEPTH = 64`, rejecting the reserved byte
`0xC1` and all ext types in v1 documents. Phase 8 builds its AST decoding on
this module.

## D10 — Pager commit releases the state lock across fsyncs

**Phase:** 4 · **Status:** accepted

`Pager::commit` originally held the pager's state lock for the whole commit,
so a reader hitting an uncached page would block behind both fsyncs —
defeating `ARCHITECTURE.md` §3.3's "readers never block on the writer".

**Decision:** commit runs in three phases: snapshot the dirty set and next
meta **under the lock**, write + fsync data and the inactive meta slot **with
the lock released**, then install the new meta and flip the active slot
**under the lock again**. Sound because the layer above guarantees a single
writer (the `txn` writer thread): no second commit can interleave, and CoW
means in-flight readers only touch pages the commit never modifies.

## D9 — `loom` as a test-only dependency gated behind `--cfg loom`

**Phase:** 4 · **Status:** accepted

The writer/reader snapshot handoff (pin, release, publish, watermark) is the
one concurrency-critical protocol in the system; sampled tests cannot prove it.
D4 keeps unvetted dependencies out of the shipped graph.

**Decision:** model-check the `Registry` with `loom`, declared under
`[target.'cfg(loom)'.dependencies]` so it is compiled **only** when
`RUSTFLAGS="--cfg loom"` is set — never in normal builds, tests, or CI gates.
The registry swaps in `loom::sync::Mutex` under that cfg; the model proves a
pinned reader can never observe its pages reclaimed, over every interleaving.
Run: `RUSTFLAGS="--cfg loom" cargo test -p txn --test loom_registry --release`.

## D8 — Transactions are validated before any mutation (validate-then-apply)

**Phase:** 4 · **Status:** accepted

`SPEC.md` requires transactions to be atomic, but a multi-op transaction whose
third op fails after two applied would need rollback — and the CoW tree has no
undo log.

**Decision:** the writer pre-validates every op (`btree::check_entry`) before
mutating anything; a validation failure rejects the **whole** transaction as a
no-op. After validation, the only remaining failures are I/O errors, which are
**fatal**: the writer aborts the uncommitted batch (nothing was durable), fans
`WriterStopped` out to every queued reply, and stops — the database stays
readable but is no longer writable. No partial transaction can ever commit.
An acknowledged commit is durable; a commit interrupted at the final meta
fsync may land either way (inherent fsync ambiguity) but never partially.

## D7 — B+tree mutations report superseded pages; the tree never frees

**Phase:** 3 · **Status:** accepted

`ARCHITECTURE.md` §3.2 says a modification "copies the touched path to a new
root" and "the `txn` layer installs it on commit", and §3.3 ties page
reclamation to live snapshots. That leaves open *who frees the old path*.

**Decision:** the B+tree is a pure transformation over pager pages and **never
frees a page itself**. `insert`/`delete` return an `Edit { new_root, freed }`,
where `freed` lists the old copied-path (and merged-sibling) pages. The
caller decides when to reclaim them — in Phase 4 the `txn` layer frees a page
only once no live snapshot needs it; that is exactly what keeps an earlier root a
valid, immutable snapshot. Phase-3 tests free eagerly when no snapshot is pinned
(to bound file growth) and skip freeing for the snapshot-isolation test.

## D6 — No leaf sibling pointers; range scans use a root-to-leaf cursor stack

**Phase:** 3 · **Status:** accepted

A classic B+tree links leaves for fast range scans. Under copy-on-write that is
costly: editing a leaf would force copying its linked neighbours (to update their
pointers), turning an O(log n) path copy into O(n) fan-out.

**Decision:** store **no sibling pointers**. A `Cursor` holds the descent path
(a stack of node + index) and advances by walking the stack — O(log
n) to cross a leaf boundary, both forward and backward. The cursor reads a fixed
root, so it is a stable snapshot for its whole life. This also makes nodes purely
parent-referenced, which is what lets an old root stay valid (see D7).

## D5 — Variable-length slotted nodes, byte-fill split/merge, provisional raw keys

**Phase:** 3 · **Status:** accepted

`ARCHITECTURE.md` §3.2 mandates an order-preserving key encoding (delivered by
`types` in Phase 5) and node split/merge, but not a concrete node layout.

**Decision:**
- **Node layout:** one node per `Data` page; a kind byte distinguishes leaf vs
  internal in the payload. Keys/values are variable length, so fill is measured
  in **bytes**: a node splits when an entry won't fit and is rebalanced (merge,
  or merge-then-split) when it drops below ¼-page. Each cell is capped at half a
  page (`MAX_CELL`) so any two cells share a page — guaranteeing a split always
  yields two non-empty halves and an internal node always holds ≥2 children. A
  single entry over the cap is a typed `EntryTooLarge` error; v1 has no overflow
  pages (deferred).
- **Decode-whole/encode-whole:** because CoW rewrites a whole node on every edit,
  nodes are decoded to an in-memory form and re-encoded rather than edited
  in-place — simpler and the cost is dwarfed by the page write.
- **Provisional keys:** keys are compared **bytewise** (raw `&[u8]`). PLAN §3
  calls for this provisional scheme; the Phase-5 order-preserving encoding will
  produce byte strings that compare identically, so the tree is unaffected.

## D4 — Seeded, model-based property tests instead of `proptest`

**Phase:** 2 · **Status:** accepted

`PLAN.md` §3.6 calls for randomized, reproducible-from-a-seed property tests.
The obvious tool is the `proptest` crate, but it (and its transitive
dependencies) would be the first third-party code to enter the dependency graph,
and the CI gate `cargo deny` (licenses/advisories) is **CI-only** — not
installed locally — so a new dependency's license/advisory status cannot be
vetted before pushing.

**Decision:** write property tests in-house against the `common::SeededRng`
(SplitMix64) host service already built in Phase 1. A test fixes a seed, drives a
randomized op sequence (alloc/free/write/commit/reopen) against the pager while a
simple in-memory model (`HashMap<page, tag>`) tracks expected contents, and
asserts the two agree plus `validate()` passes. Seeds are listed explicitly so a
failure is reproducible. This keeps the dependency graph empty of unvetted crates
while satisfying §3.6. Revisit if shrinking (minimal counterexamples) becomes
worth a dependency.

## D3 — In-house software CRC32C (Castagnoli), no dependency

**Phase:** 2 · **Status:** accepted

`ARCHITECTURE.md` specifies a CRC32C per-page checksum. Crates such as `crc32c`
or `crc` would pull in third-party code that, per D4's reasoning, cannot be
license/advisory-vetted locally (the `cargo deny` gate is CI-only).

**Decision:** implement CRC32C (Castagnoli polynomial `0x82F63B78`) in `pager`
as a small, table-driven software routine (`crc::crc32c`), with the lookup table
built by a `const fn` at compile time. Correctness is pinned by the standard
check vector (`crc32c(b"123456789") == 0xE3069283`). No hardware-intrinsic
(SSE4.2) path for now — portability and a zero-dependency graph over peak
throughput; revisit if checksum cost shows up in profiling.

## D2 — Public crate is `otf-dbms`; internal crates keep functional names

**Phase:** 1 · **Status:** accepted

`ARCHITECTURE.md` §2 names the public-API crate `core`. A Rust crate named
`core` collides with the standard library's built-in `core` crate (both land in
the extern prelude → ambiguous-name compile errors in every dependent and in the
crate's own doc-tests). The names are explicitly placeholders ("rename when a
product name is chosen").

**Decision:** namespace the **public** crate under the org and keep the internal
crates short:

- Public crate: package **`otf-dbms`**, directory `crates/dbms`, imported in
  code as **`otf_dbms`**. This both carries the org namespace and removes the
  `core`/std collision.
- Internal crates keep their functional names (`common`, `pager`, `btree`,
  `txn`, `types`, `catalog`, `index`, `proto`, `query`) and are unpublished
  (`publish = false`) path dependencies.
- The CLI binary is named **`otf-dbms`** (package `cli`).

`ARCHITECTURE.md` §2 updated to reflect the `core` → `otf-dbms` rename.

## D1 — Added an 11th crate, `common`, for cross-cutting foundations

**Phase:** 1 · **Status:** accepted

`ARCHITECTURE.md` §2 lists ten crates, none an obvious home for genuinely
cross-cutting concerns: the `SPEC.md` §9 error-category taxonomy and the
injectable `Clock` / `Rng` / `IoBackend` host services shared by
`pager`/`btree`/`txn`. Burying them in `pager` would force unrelated upper
crates (e.g. `proto`) to depend on the storage layer just to reach an error
category or a clock — backwards coupling.

**Decision:** introduce a tightly-scoped `common` crate at the **bottom** of the
stack (below `pager`). It contains **only**:

1. `ErrorCategory` (the §9 taxonomy) and a `CategorizedError` trait
   (`fn category(&self) -> ErrorCategory`). Each crate keeps its own `thiserror`
   enum and implements the trait; `otf-dbms` aggregates them. This preserves
   "one error enum per crate" while sharing the taxonomy.
2. The `Clock`, `Rng`, and `IoBackend` traits.
3. The three `IoBackend` implementations (real-file, in-memory, fault-injecting)
   — shared test/simulation infrastructure.

**Explicitly kept out** of `common`: domain newtypes (`PageId`, `TxnId`,
`Value`, …) stay in their owning crates so `common` does not become a junk
drawer. `ARCHITECTURE.md` §1 diagram and §2 table updated to add `common`.
