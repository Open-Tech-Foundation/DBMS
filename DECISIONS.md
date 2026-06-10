# Decisions log

Per `PLAN.md` §1 rule 6, every resolution of an ambiguity or deviation from
`SPEC.md` / `ARCHITECTURE.md` is recorded here with its rationale. Newest first.

---

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
