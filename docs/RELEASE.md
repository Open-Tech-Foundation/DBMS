# RELEASE — Production readiness & ecosystem plan

Status: draft roadmap for taking OTF DBMS from the completed v1 engine (Phases
1–10, Phase 11 in progress) to a published, production-usable release in both
the Rust and JavaScript ecosystems.

## Positioning (read first)

v1 is a deliberately **narrow** embedded engine: a memory-safe, copy-on-write
relational core with a **structured binary query protocol** (typed AST /
MessagePack), MVCC snapshot isolation, and built-in resource limits. It is
**not** a SQLite feature replacement. No SQL text front-end, foreign keys,
encryption, or network host in v1 (see `SPEC.md` §11, `PLAN.md` §8). Release
messaging must say this plainly so scope gaps aren't filed as bugs.

Choose OTF for a small, safe, embeddable engine with a binary protocol and no
SQL-injection surface, sharing one AST across Rust and JS. Choose SQLite for the
full relational feature set and 20+ years of hardening today.

## Track 1 — Finish hardening (close Phase 11)

- [ ] Criterion benches with a **committed baseline**: point-get, range scan,
      bulk insert, mixed read/write under a concurrent writer. Required before
      any published performance comparison.
- [ ] Promote `proto_fuzz` to a **nightly cargo-fuzz** job (not just unit cases).
- [ ] All `PLAN.md` §7 acceptance scenarios green **in CI**.
- [ ] Extend crash/power-loss injection over the free-list rebuild path.
- [ ] Measure and record binary size (workspace + published crate).

## Track 2 — Rust release engineering

- [ ] Fix MSRV: `rust-version = "1.95"` is not a real release — set the true
      minimum and add a `cargo +<msrv>` CI job so it can't rot.
- [ ] Decide the **publish surface**. crates.io requires every dependency to be
      published, so shipping `otf-dbms` means publishing all internal crates
      (or collapsing them). Recommendation: publish `otf-dbms` (public API) +
      `otf-dbms-cli`; publish the rest as "internal, no semver guarantee" with a
      doc note. Flip `publish = true` selectively.
- [ ] Set `version = "0.1.0"`; add `cargo-semver-checks` to CI.
- [ ] Public-API stability pass: `#[non_exhaustive]` on public enums (`Error`,
      `Value`, `Request`, `Stage`), sealed traits where external impls aren't
      wanted.
- [ ] `SECURITY.md`; commit to a **file-format compat policy** (SPEC §10 defines
      magic + version — freeze v1, reject unknown format versions on open).
- [ ] docs.rs metadata, README badges, confirm README doctest still compiles.

## Track 3 — JavaScript / TypeScript ecosystem

The MessagePack wire protocol is the enabling asset: the FFI boundary is
"request bytes in, response bytes out," so every binding wraps one function.

- [ ] **Node/Bun native addon** via napi-rs (N-API). File-backed, best perf.
      Ship **prebuilt binaries** for `{linux,darwin,win} × {x64,arm64}` on npm so
      users need no Rust toolchain.
- [ ] **WASM** via wasm-bindgen for browsers, Deno, Bun, edge (Workers). Memory
      first; then **OPFS**-backed persistence (async API).
- [ ] **`@opentf/dbms` TS wrapper**: a typed AST builder emitting the same
      `Request` shape (PLAN §8.5) + typed `Row`/`Response` accessors, `.d.ts`
      shipped. Single async surface shared by native and WASM backends.
- [ ] CI matrix builds/tests both targets; publish on tag.

## Track 4 — Governance & docs

- [ ] CONTRIBUTING, CODE_OF_CONDUCT, issue templates.
- [ ] Versioned docs site from SPEC/ARCHITECTURE/PLAN/DECISIONS.
- [ ] A "what v1 is / isn't" page linking the SPEC §11 scope table.

## Milestones

- **0.1.0** — Tracks 1+2: shippable, honest embedded engine on crates.io.
- **0.2.0** — Track 3 (Node native): `@opentf/dbms` on npm.
- **0.3.0** — Track 3 (WASM/OPFS): browser + edge story.
- Track 4 runs continuously.

## OTF DBMS (v1) vs SQLite

| Dimension | OTF DBMS (v1) | SQLite |
|---|---|---|
| Category | Embedded, single-file relational | Embedded, single-file relational |
| Implementation | Rust (no `unwrap`/`panic` in lib, enforced) | C |
| Memory safety | Safe by construction | Manual; decades-hardened |
| Query interface | Structured binary AST (typed / MessagePack) | SQL text |
| Storage engine | Copy-on-write B+tree, checksummed pages | B-tree, rollback journal / WAL |
| Durability | Double-buffered meta page, atomic commit | Rollback journal or WAL file |
| Crash safety | ACID; recovers to last commit | ACID; recovers to last commit |
| Concurrency | MVCC snapshot, single writer, non-blocking readers | Single writer; concurrent readers (WAL) |
| Isolation | Snapshot isolation | Serializable (WAL: reader snapshot) |
| Cursor stability | Snapshot-owning keyset cursor | Depends on journal mode |
| Typing | Static typed columns | Dynamic (type affinity) |
| Types | null, bool, i64, f64, text, blob, uuid, json, timestamp | null, int, real, text, blob (JSON via ext) |
| Constraints | PK, NOT NULL, UNIQUE, CHECK, DEFAULT, FK, auto-inc, generators, rowversion | PK, NOT NULL, UNIQUE, CHECK, DEFAULT, FK |
| Foreign keys | RESTRICT/CASCADE/SET NULL on delete + update; composite/self-ref | Yes (all actions) |
| Indexes | B+tree single/composite/unique, auto-maintained | B+tree, partial, expression, covering |
| Joins | INNER, LEFT, CROSS (nested-loop, index-assisted) | + RIGHT/FULL, subqueries, CTEs, windows |
| Aggregates | COUNT/SUM/MIN/MAX/AVG + GROUP/HAVING | Full + window functions |
| Extensions | None (v1) | FTS5, R-tree, JSON1, large ecosystem |
| Safety limits | Per-query caps (rows/joins/memory/deadline), bounded decode | Configurable limits API |
| Encryption at rest | No (v1; planned) | SEE / SQLCipher |
| Network / server | No (v1; D1-style host planned) | No (embedded only) |
| Rust binding | Native crate (`otf-dbms`) | `rusqlite` / `sqlx` (FFI to C) |
| JS binding | Planned: napi native + WASM/OPFS, typed AST SDK | better-sqlite3, node:sqlite, sql.js, wa-sqlite |
| Maturity | Pre-1.0 | 20+ years, ~trillion deployments |
| License | Apache-2.0 | Public domain |
