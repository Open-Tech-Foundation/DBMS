# Decisions log

Per `PLAN.md` §1 rule 6, every resolution of an ambiguity or deviation from
`SPEC.md` / `ARCHITECTURE.md` is recorded here with its rationale. Newest first.

---

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
