# Benchmarks

The criterion bench suite (`PLAN.md` §3.8) lives in
`crates/dbms/benches/engine.rs` and runs through the public `otf_dbms` API on an
**in-memory** backend, so the numbers reflect the engine — CoW B+tree, MVCC
commit, planner, executor — not disk latency.

## Running

```sh
# Full suite.
cargo bench -p otf-dbms --bench engine

# Save a named baseline, then compare a later run against it.
cargo bench -p otf-dbms --bench engine -- --save-baseline main
cargo bench -p otf-dbms --bench engine -- --baseline main
```

CI compiles the benches on every push (via `build --all-targets`) but only
*runs* them on demand — trigger the **benchmarks (on demand)** job from the
Actions tab (`workflow_dispatch`); shared runners are too noisy for trustworthy
timings.

## What is measured

| Bench | What it exercises |
|---|---|
| `point_read/pk_lookup` | `WHERE id = k` on the primary key → a base-tree point lookup (`Plan::PkLookup`) |
| `point_read/secondary_eq` | `WHERE val = k` on an indexed non-PK column — still a full scan + filter (see below) |
| `full_scan/100k` | scan + materialize every row of a 100k-row table |
| `insert_1k/serial` | 1 000 single-row inserts = 1 000 transactions |
| `insert_1k/batch` | the same 1 000 rows in one request = one transaction |
| `guarded_update` | a guarded relative update (read-check-write inside the writer) |
| `inner_join_group_2k` | emp ⋈ dept ⋈ region (2 000 / 50 / 5) + GROUP BY + COUNT |
| `group_by/sum_50k_over_20` | SUM over 50k rows grouped into 20 buckets |

## Baseline

Machine: Intel i7-8700K @ 3.7 GHz, Linux 6.12, rustc 1.95.0, release build.
Numbers are indicative on shared hardware — treat criterion's saved baselines as
the source of truth for regression comparisons.

<!-- BASELINE:START -->
| Access path | Cost per op (100k-row table) |
|---|---|
| `point_read/pk_lookup` (PK point lookup) | **~19 µs** |
| `point_read/secondary_eq` (non-PK equality, full scan) | ~34 ms |

The primary-key point lookup is ~1700× faster than the equivalent full scan on a
100k-row table — the direct payoff of the `Plan::PkLookup` access path (D33).
<!-- BASELINE:END -->

## Reading the baseline

- **PK equality is now a point lookup.** A `WHERE pk = k` that pins every
  primary-key column plans as `Plan::PkLookup` and executes as a single base-tree
  `get` — O(log n), ~19 µs above (D33).
- **Secondary-index equality is still a full scan** — the next access-path
  optimization. `query::plan::index_select` emits `Plan::IndexScan`, but the
  executor (`exec::scan`) still materializes the whole table and filters to the
  index prefix rather than probing the index tree. Teaching the executor to seek
  the index tree (values → PK keys → base rows) would give secondary lookups the
  same O(log n) the PK lookup now has.
- `insert_1k/serial` vs `insert_1k/batch` is the per-transaction commit
  overhead. On this in-memory backend it isolates the CoW + meta-swap cost; a
  file backend would add fsync latency per commit (a future file-backed
  throughput bench).
