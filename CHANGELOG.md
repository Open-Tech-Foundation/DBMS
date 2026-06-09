# Changelog

All notable changes are recorded here, in [Keep a Changelog][kac] style,
organized under a per-phase heading (see `PLAN.md` §2). Each entry is one line
under a category (`Added` / `Changed` / `Fixed` / `Removed` / `Security`).

[kac]: https://keepachangelog.com/en/1.1.0/

## [Unreleased]

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
