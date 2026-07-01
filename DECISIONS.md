# Decisions log

Per `PLAN.md` §1 rule 6, every resolution of an ambiguity or deviation from
`SPEC.md` / `ARCHITECTURE.md` is recorded here with its rationale. Newest first.

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
