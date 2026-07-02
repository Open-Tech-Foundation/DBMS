# Benchmarks

The criterion bench suite (`PLAN.md` §3.8) lives in
`crates/edb/benches/engine.rs` and runs through the public `otf_edb` API on an
**in-memory** backend, so the numbers reflect the engine — CoW B+tree, MVCC
commit, planner, executor — not disk latency.

## Running

```sh
# Full suite.
cargo bench -p otf-edb --bench engine

# Save a named baseline, then compare a later run against it.
cargo bench -p otf-edb --bench engine -- --save-baseline main
cargo bench -p otf-edb --bench engine -- --baseline main
```

CI compiles the benches on every push (via `build --all-targets`) but only
*runs* them on demand — trigger the **benchmarks (on demand)** job from the
Actions tab (`workflow_dispatch`); shared runners are too noisy for trustworthy
timings.

## What is measured

| Bench | What it exercises |
|---|---|
| `point_read/pk_lookup` | `WHERE id = k` on the primary key → a base-tree point lookup (`Plan::PkLookup`) |
| `point_read/secondary_eq` | `WHERE val = k` on an indexed non-PK column → an index-tree seek |
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
| Access path | Cost per op | vs. full scan |
|---|---|---|
| `point_read/pk_lookup` — PK point lookup (100k rows) | **~19 µs** | ~1700× |
| `point_read/secondary_eq` — indexed non-PK seek (10k rows) | **~135 µs** | ~25× |

Both equality access paths are now O(log n) index/base-tree seeks rather than
full scans (D33). The PK figure is at 100k rows; the secondary figure is at 10k
(index-build setup dominates a 100k measurement — see the note on write costs
below). The scan they replace is O(rows), so the speedup grows with table size.
<!-- BASELINE:END -->

## Reading the baseline

- **PK equality is a point lookup.** A `WHERE pk = k` that pins every primary-key
  column plans as `Plan::PkLookup` and executes as a single base-tree `get` —
  O(log n) (D33).
- **Secondary-index equality is an index seek.** `Plan::IndexScan` is served by
  `CatSnapshot::index_candidates`: the index tree is range-probed for the
  matching primary keys, the base rows are fetched, and the exact equality is
  applied — O(log n + matches) instead of a full scan.
- **Known slow paths (next targets):** large batch `insert` and `create_index`
  backfill are slow at scale (a 10k-row unique-index build is ~1.4 s), which is
  why the secondary bench is measured at 10k. These are separate write-path
  optimizations, independent of the read access paths above.
- `insert_1k/serial` vs `insert_1k/batch` is the per-transaction commit
  overhead. On this in-memory backend it isolates the CoW + meta-swap cost; a
  file backend would add fsync latency per commit (a future file-backed
  throughput bench).
