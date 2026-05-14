# Localnet test results

Runs of the QA suite (see [README.md](README.md)) against a local
`iota-localnet` (the `dev-tools/pg-services-local/docker-compose.yaml` stack)
on a 2026-vintage MacBook (arm64, 16 threads).

These runs are what the localnet scripts are tuned for: small DBs, fast
iteration, and the workload generator's 8-scenario synthetic data used as
seed for tests 1 and 2. For staging runs see [`staging_results.md`](staging_results.md).

## Environment

| | |
|---|---|
| Localnet | `iota-localnet` in docker, single validator + access fullnode, 120 s/epoch |
| Pruning disabled | fullnode + per-validator + `network.yaml` patched via the docker-compose `sed` so the gRPC checkpoint stream stays healthy for long runs |
| QA binaries | built on `sc-platform/consistent-views-qa` (flag + snapshot graceful-exit + 5 s PRUNING_DELAY_MS + writer drain) |
| qa-workload | Rust binary at `target/release/qa-workload` — publishes a Move package and runs 8 scenarios in parallel, one gas coin each |

## Workload generation (prerequisite for tests 1, 2)

```sh
./target/release/qa-workload \
  --rpc-url http://127.0.0.1:9000 \
  --faucet-url http://localhost:9123/gas \
  --package-path consistent_views_qa_scripts/workload/move_package \
  --output /tmp/workload_manifest.json
```

Takes ~5 min. Produces JSON with `final_checkpoint` + grouped object IDs:
- 10 parents × 10 DFs
- 10 parents × 10 DOFs
- 3 parents with add → remove → add lifecycle
- 10 objects transferred to a synthetic recipient
- 10 children wrap → unwrap
- 10 children wrap + delete (tombstone)
- 10 parents plain-deleted
- 3 borrow_mut quirk pairs (parent + DOF child)

## Test 1 — Migration parity ✅

| Phase | Time |
|---|---|
| OLD sync 0 → cp 600 | seconds |
| NEW sync 0 → cp 600 (with backward writes) | seconds |
| catch-up loop (in-flight commit batching race) | 17 iterations to converge |
| migration apply on db_a | sub-second |
| diff 3.1M rows of `checkpointed_objects` | seconds |

Result on the last run:
- catch-up converged at cp 1246 after 8 attempts (advance-only-the-trailing-DB strategy)
- sanity check: `db_a` no `checkpointed_objects` table (OLD), `db_b` has 9 867 rows (NEW)
- `availableRange` db_a: error "outside the available range" — migrated indexer correctly refuses (no backward coverage)
- `availableRange` db_b: `first=346 last=1246` — capped by `BACKWARD_HISTORY_MAX_LOOKBACK=900`
- rowcount: 9 867 = 9 867
- md5: `24aeea44054298d6de33792d08376208` (both)
- **PARITY: identical** • EXIT=0

## Test 2 — Query parity ✅

Two indexers (OLD-synced `query_parity_old`, NEW-synced `query_parity_new`),
two graphql-rpc readers (forward built from `--base-branch`, backward built
from `--backward-branch`), parametric query suite paginated.

Catch-up converged at cp 1246 after 8 iterations.

| Query | nodes (est) | pages | match? |
|---|---|---|---|
| `object.dynamicFields(df_parent)` | 10 DFs | 2 | ✓ |
| `object.dynamicFields(dof_parent)` | 10 DOFs | 2 | ✓ |
| `objects(filter: { type: pkg::workload::Parent })` | ~36 parents | 8 | ✓ |
| `address.coinObjects` | 10 coins | 1 | ✓ |
| 12 × `object(v=cur|cur−1|cur−5|3).dynamicFields(parent)` | 0–10 DFs | 12–24 | ✓ |
| 3 × `object(wrapped_then_deleted=X)` | tombstone | 3 | ✓ |
| `objects(filter: { owner=0xab…ab })` | 10 transferred | 2 | ✓ |

**Total: 36 pages match, 0 differ** • EXIT=0

Version-pinned probes hit real object versions: e.g. parent `0xbc91b42c…` has versions `{4, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23}`; probes at `{3, 18, 22, 23}` exercise both the null path (v=3, before mint) and active states (v=18, 22, 23).

## Test 3 — Pruning ✅

`pruning_test.py` syncs NEW with `--pruning-config-path` configured for
`epochs_to_keep=2` on `objects_backward_history` (and `objects_history`),
dwells for 30 s, then asserts.

After dwell at cp 9 756:
- `objects_backward_history.min_available_cp = 8890` (advanced from 0)
- `MIN(superseded_at_checkpoint) FROM objects_backward_history = 8890`
- Row count: 1 722 (smaller than 0 — pruned)
- GraphQL `availableRange.first = 8890` (matches the pruning floor; the
  `BACKWARD_HISTORY_MAX_LOOKBACK=900` cap would have given 8856 — pruning
  watermark dominates)
- EXIT=0

## Test 4 — Pruning after migration ✅

`pruning_after_migration_test.py` syncs OLD to cp ≈ 2950, then runs NEW with
pruning enabled for 180 s. The migration seeds
`objects_backward_history.min_available_cp = settled + 1`; we then verify
pruning correctly resumes from that seed.

After dwell at cp 9 443:
- `objects_backward_history.min_available_cp` advanced from migration seed (~2950) → **9345**
- 191 remaining rows; all have `superseded_at_checkpoint ∈ [9345, 9443]`
- **Boundary row present** at exactly `superseded_at_checkpoint = 9345` (no off-by-one dropping it)
- `availableRange.first = 9345`
- EXIT=0

## Test 5 — Migration on big DB

Not run on localnet (the bigger DB is on staging). See
[`staging_results.md`](staging_results.md#test-5--migration-on-big-db) and
the prior benchmark in `pruning_qa_scripts/checkpointed_objects_migration_benchmark.md`.

## How to reproduce locally

```sh
# 1. start postgres + localnet
cd dev-tools/pg-services-local && docker compose up -d local-network postgres && cd -

# 2. build qa-workload
cargo build --release -p qa-workload

# 3. generate workload
./target/release/qa-workload \
  --rpc-url http://127.0.0.1:9000 \
  --faucet-url http://localhost:9123/gas \
  --package-path consistent_views_qa_scripts/workload/move_package \
  --output /tmp/workload_manifest.json

# 4. run any test (substitute the script path)
python3 consistent_views_qa_scripts/migration_parity/migration_parity.py \
  --manifest /tmp/workload_manifest.json \
  --pg-url postgres://postgres:postgrespw@localhost:5432 \
  --remote-store-url http://localhost:50051
```

`--reuse-worktrees --skip-rebuild` on subsequent runs.
