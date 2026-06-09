# Changelog

All notable changes are recorded here, in [Keep a Changelog][kac] style,
organized under a per-phase heading (see `PLAN.md` §2). Each entry is one line
under a category (`Added` / `Changed` / `Fixed` / `Removed` / `Security`).

[kac]: https://keepachangelog.com/en/1.1.0/

## [Unreleased]

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
