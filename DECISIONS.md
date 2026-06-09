# Decisions log

Per `PLAN.md` §1 rule 6, every resolution of an ambiguity or deviation from
`SPEC.md` / `ARCHITECTURE.md` is recorded here with its rationale. Newest first.

---

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
