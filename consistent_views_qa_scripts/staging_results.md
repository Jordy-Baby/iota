# Staging test results

Runs of the QA suite (see [README.md](README.md)) against the staging cluster.

## Environment

| | |
|---|---|
| Host | `staging` (Ubuntu 24.04, 96-thread, NVMe) |
| Validators | `validator-0/1/2` (iota-node 1.21.0-alpha) |
| Fullnode | `access-0` exposing JSON-RPC :9000, gRPC :50051 |
| Postgres | docker container, `iota_indexer/iota_indexer` user, `iota_indexer` DB pre-populated 20 GB |
| `qa/backward-merge-base` branch | `9431493335` — the merge base of `develop` and `infra/feat/backward-history-consistent-views`; used as `--base-branch` so the test isolates the effect of the backward-feature commits only |

The QA branch (`sc-platform/consistent-views-qa`) is pushed to the staging repo at `/root/repositories/tomxey/iota`.

## Test 1 — Migration parity

`migration_parity.py` builds an OLD-code (merge-base) indexer and a NEW-code
(backward-feature) indexer, syncs both from genesis to a target checkpoint
into two databases, applies the new Diesel migration to the OLD DB, and
diffs `checkpointed_objects` byte-for-byte.

### Run 1 — `--stop-at-checkpoint 150000` (0.7% of staging tip)

| Phase | Time |
|---|---|
| 0 — prepare binaries (first build) | 202.9s |
| 1 — reset 2 DBs | 0.3s |
| 2 — sync both indexers 0 → 150 000 | **754.4s (12:34)** |
| 3 — apply NEW migration on db_a | **93.8s** |
| 4a — graphql `availableRange` check | 2.1s |
| 4b — compare `checkpointed_objects` (3.1M rows) | 29.5s |

Result:
- catch-up loop: **0 iterations** (both DBs hit 150 000 exactly first try)
- `checkpointed_objects` rowcount: **3 101 704** on both DBs
- md5: `bae553e39d7c5dbee8f7ad79578d5fe9` (both)
- `availableRange` db_a (migrated): error "outside the available range" → migrated indexer correctly reports no backward coverage
- `availableRange` db_b (from-scratch): `first=149100 last=150000` — capped by `BACKWARD_HISTORY_MAX_LOOKBACK` (900)
- **PARITY: identical** • EXIT=0

### Run 2 — `--stop-at-checkpoint 1000000` (4.6% of staging tip)

| Phase | Time |
|---|---|
| 0 — prepare binaries (cache hit) | 0.0s |
| 1 — reset 2 DBs | 20.3s |
| 2 — sync both indexers 0 → 1 000 000 | **1830.5s (30:31)** |
| 3 — apply NEW migration on db_a | **94.0s** |
| 4a — graphql `availableRange` check | 2.1s |
| 4b — compare `checkpointed_objects` (3.1M rows) | 30.3s |

Result:
- catch-up loop: **0 iterations** (both at 1 000 000 first try)
- `checkpointed_objects` rowcount: **3 101 716** on both DBs (just 12 more rows than the 150K run — migration is row-count-bound, not cp-count-bound)
- md5: `96ca5f5b14dd93baf71fd1f68d01e767` (both)
- `availableRange` db_b: `first=999100 last=1000000` (same 900-cp cap)
- **PARITY: identical** • EXIT=0

## Test 5 — Migration on big DB

Captured directly from the migration parity runs above:

| target cp | `objects_snapshot` watermark | `checkpointed_objects` rows | migration time |
|---|---|---|---|
| 150 000 | trailing the cp by ~snapshot_min_lag | 3 101 704 | **93.8s** |
| 1 000 000 | trailing the cp by ~snapshot_min_lag | 3 101 716 | **94.0s** |

**Headline:** the migration cost is **bounded by row count in `objects_snapshot` + the recent `objects_history` delta**, not by cp count. Going 6.7× more checkpoints (150K → 1M) added 12 rows and 0.2s. To stress the migration further we'd need a cp range that actually mints many new objects (e.g. a higher cp range on staging where on-chain activity is denser).

Prior data point (from `pruning_qa_scripts/checkpointed_objects_migration_benchmark.md`, Apr 8):
- **8m28s** on 15.2M synthetic objects matching mainnet distribution.

So the migration scales roughly **5× the row-count → 5× the time** (3.1M × 8m28s/15.2M ≈ 1m44s expected, observed 1m34s — consistent).

## How to reproduce

On the staging host with the QA branch checked out:

```sh
cd /root/repositories/tomxey/iota
export PATH=/root/.cargo/bin:$PATH
python3 -u consistent_views_qa_scripts/migration_parity/migration_parity.py \
  --stop-at-checkpoint <N> \
  --base-branch qa/backward-merge-base \
  --pg-url postgres://iota_indexer:iota_indexer@localhost:5432 \
  --remote-store-url http://localhost:50051 \
  --catchup-attempts 100 \
  --reuse-worktrees --skip-rebuild \
  --keep-dbs
```

Wrap in `tmux new-session -d -s parity '…'` so it survives ssh disconnects.

The `qa/backward-merge-base` local branch is created on staging with:

```sh
git branch qa/backward-merge-base $(git merge-base develop infra/feat/backward-history-consistent-views)
```
