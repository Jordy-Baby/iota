# Consistent-views QA suite

End-to-end tests for the backward-history consistency model
(issue [#11500](https://github.com/iotaledger/iota/issues/11500)).
Six scripts, one per test plan item plus a perf probe:

| # | Test | Script |
|---|---|---|
| 1 | Migration parity | [`migration_parity/migration_parity.py`](migration_parity/migration_parity.py) |
| 2 | Query parity (forward vs backward graphql) | [`query_parity/query_parity.py`](query_parity/query_parity.py) |
| 3 | Pruning | [`pruning/pruning_test.py`](pruning/pruning_test.py) |
| 4 | Pruning after migration | [`pruning_after_migration/pruning_after_migration_test.py`](pruning_after_migration/pruning_after_migration_test.py) |
| 5 | Migration on a big DB | manual; benchmark notes in `../pruning_qa_scripts/checkpointed_objects_migration_benchmark.md` |
| 6 | Query performance (cursor-pinned + version-pinned) | [`query_perf/query_perf.py`](query_perf/query_perf.py) |

The scripts rely on test-only indexer changes that live on the
`sc-platform/consistent-views-qa` branch (the "flag branch"):

- `--stop-at-checkpoint=N` on `iota-indexer indexer` for deterministic stops.
- Snapshot pipeline exits gracefully on cancel during its initial-lag wait.
- `PRUNING_DELAY_MS` shrunk from 2h to 5s so pruning fires within a test
  timeframe.
- Primary writer drains the in-flight channel on cancel so the DB watermark
  matches the stop point exactly.

Without those changes the scripts won't behave deterministically.


## Prerequisites

- macOS or Linux with Docker, `cargo`, `psql`, `python3` (≥3.10).
- The three local branches the scripts cherry-pick / build from must exist
  in your repo clone:
  - `develop` — used as the OLD-code baseline.
  - `infra/feat/backward-history-consistent-views` — the NEW (backward-history)
    feature branch.
  - `sc-platform/consistent-views-qa` — the QA flag branch (this branch).
- Sibling worktrees managed automatically under `../migration_parity_worktrees/`.
- A workload manifest (used by tests 1, 2 to seed object IDs / target cp).


## One-time setup

### 1. Start postgres + localnet

```sh
cd dev-tools/pg-services-local
docker compose up -d local-network postgres
```

`docker-compose.yaml` on this branch disables object pruning on the fullnode,
the per-validator config, and `network.yaml`. Without that the localnet's gRPC
checkpoint stream eventually fails to reconstruct historical checkpoints.

Wait until the localnet is healthy:

```sh
docker compose ps local-network
# should show "(healthy)"
```

### 2. Build the qa-workload binary

```sh
cd <repo-root>
cargo build --release -p qa-workload
```

This builds the parallel workload generator at
`target/release/qa-workload`. The Move package it publishes lives at
`consistent_views_qa_scripts/workload/move_package/`.

### 3. Build the indexer + graphql-rpc once for each branch

The scripts auto-create git worktrees and build the binaries on first run.
Expect ~5–10 min the first time per worktree. Subsequent runs reuse the
binaries via `--skip-rebuild`.

You can pre-build manually if you prefer:

```sh
git worktree add -b qa/base   ../migration_parity_worktrees/develop                                       develop
git worktree add -b qa/bwd    ../migration_parity_worktrees/infra_feat_backward-history-consistent-views  infra/feat/backward-history-consistent-views
for wt in ../migration_parity_worktrees/develop ../migration_parity_worktrees/infra_feat_backward-history-consistent-views; do
  (cd $wt && git cherry-pick sc-platform/consistent-views-qa && \
   cargo build --release -p iota-indexer -p iota-node -p iota-graphql-rpc)
done
```


## Running the tests

All commands assume cwd = repo root.

### 0. Generate a workload (needed by tests 1, 2)

```sh
./target/release/qa-workload \
  --rpc-url http://127.0.0.1:9000 \
  --faucet-url http://localhost:9123/gas \
  --package-path consistent_views_qa_scripts/workload/move_package \
  --output /tmp/workload_manifest.json
```

Takes ~5 min — runs 8 scenarios in parallel using one dedicated gas coin each.
Produces a JSON manifest at `/tmp/workload_manifest.json` with the final
checkpoint number and the object IDs grouped by scenario.

### 1. Migration parity

```sh
python3 consistent_views_qa_scripts/migration_parity/migration_parity.py \
  --manifest /tmp/workload_manifest.json \
  --pg-url postgres://postgres:postgrespw@localhost:5432 \
  --remote-store-url http://localhost:50051
```

Pass: `PARITY: checkpointed_objects identical` + `EXIT=0`. Also asserts the
GraphQL `availableRange` invariant — migrated indexer refuses backward-history
queries (no coverage), from-scratch indexer has full coverage.

Iteration: append `--reuse-worktrees --skip-rebuild` after the first run.

### 2. Query parity

```sh
python3 consistent_views_qa_scripts/query_parity/query_parity.py \
  --manifest /tmp/workload_manifest.json \
  --pg-url postgres://postgres:postgrespw@localhost:5432 \
  --remote-store-url http://localhost:50051
```

Pass: `summary: <N> pages match, 0 differ` + `EXIT=0`. Runs the full query
suite (current DFs/DOFs, type-filtered list, coin pagination, version-pinned
DFs at multiple historical versions, tombstone lookups, owner-filtered list)
paginating each, comparing JSON responses byte-for-byte between the forward
and backward graphql readers.

### 3. Pruning

```sh
python3 consistent_views_qa_scripts/pruning/pruning_test.py \
  --pg-url postgres://postgres:postgrespw@localhost:5432 \
  --remote-store-url http://localhost:50051
```

Pass: `OK: availableRange.first=<N> >= pruning watermark <N>` + `EXIT=0`.
Verifies rows actually removed, watermarks advanced, GraphQL `availableRange`
honours the pruning floor.

### 4. Pruning after migration

```sh
python3 consistent_views_qa_scripts/pruning_after_migration/pruning_after_migration_test.py \
  --pg-url postgres://postgres:postgrespw@localhost:5432 \
  --remote-store-url http://localhost:50051
```

Pass: `boundary row present — no off-by-one` + `EXIT=0`. Verifies that
pruning resumes correctly from the migration-set watermark seed, advancing
the floor without missing the boundary row.

### 5. Migration on big DB

Manual. The migration is two SQL `INSERT`s in
`crates/iota-indexer/migrations/pg/2026-04-02-120000_checkpointed_objects/up.sql`.
Run them with `\timing` on a staging-sized DB and record the elapsed time.
Last measurement (Apr 8 2026, 15.2M synthetic objects matching mainnet
distribution) is **8m28s** — see
`pruning_qa_scripts/checkpointed_objects_migration_benchmark.md` for the full
setup.

### 6. Query performance

Standalone perf probe — does *not* spin up its own indexer/graphql. Issues
paginated `objects(...)` queries against a running NEW `iota-graphql-rpc`,
sweeping the cursor's encoded `checkpoint_viewed_at` (BCS-crafted client-side
so you don't need to wait in real time) and the parent/object versions.
Reports p50 / p95 / max per bucket.

```sh
# 1. Start a NEW iota-graphql-rpc against an existing NEW-indexed DB
.../target/release/iota-graphql-rpc start-server \
  --port 9125 --host 0.0.0.0 \
  --db-url postgres://<user>:<pwd>@localhost:5432/<db>

# 2. Run the perf script
python3 consistent_views_qa_scripts/query_perf/query_perf.py \
  --url http://localhost:9125/graphql \
  --shape all   # ids | type | owner | empty | version-pin | object-keys | dynamic-fields | all
```

Eight shapes are supported; each can be parameterised with the worst-case
filter constants for your environment (`--type`, `--owner`, `--ids`,
`--version-pin-address`, `--df-parent-address`, `--df-latest-version`).
Defaults are the staging worst-case constants. Tests should be run with the
indexer **stopped** so `latest_cp` is stable — otherwise the lookback window
moves between probe and query and deep-K cursor-pinned samples may fall
outside it.


## Running on staging

The QA branch is pushed to the staging host's repo (`/root/repositories/tomxey/iota`).
On staging:

```sh
ssh staging
cd /root/repositories/tomxey/iota
export PATH=/root/.cargo/bin:$PATH
```

Differences from localnet:

- **No localnet docker stack** — staging has real validators (`validator-0/1/2`)
  and a fullnode (`access-0`) on the host. Postgres is in a docker container
  at `localhost:5432`. Use `postgres://iota_indexer:iota_indexer@localhost:5432`
  and `--remote-store-url http://localhost:50051`.
- **No workload generation** — staging has its own traffic; tests 1 and 2 use
  `--stop-at-checkpoint <N>` instead of a workload manifest, and the manifest
  flags can be omitted.
- **`qa/backward-merge-base` branch** — to isolate the test to the
  backward-feature commits only (excluding unrelated drift on `develop`), the
  parity tests are run with `--base-branch qa/backward-merge-base`. Create
  this local branch once:
  ```sh
  git branch qa/backward-merge-base \
    $(git merge-base develop infra/feat/backward-history-consistent-views)
  ```
- **Long runs** — wrap each test in `tmux new-session -d -s <name> '...'` so it
  survives ssh disconnects.

Example: migration parity at cp 150 000:

```sh
tmux new-session -d -s parity 'python3 -u \
  consistent_views_qa_scripts/migration_parity/migration_parity.py \
    --stop-at-checkpoint 150000 \
    --base-branch qa/backward-merge-base \
    --pg-url postgres://iota_indexer:iota_indexer@localhost:5432 \
    --remote-store-url http://localhost:50051 \
    --catchup-attempts 100 \
    --reuse-worktrees --skip-rebuild \
    --keep-dbs \
    > /tmp/migration_parity.log 2>&1'
```

Captured results live in [`staging_results.md`](staging_results.md).

For test 6 on staging see also the heavy-data inspection of
`objects_backward_history` to pick worst-case filter constants — the defaults
already hard-code the staging worst case (Random's inner Versioned as the
`dynamic-fields` parent, Clock as the `version-pin` / `object-keys` target,
etc.).


## Tearing down

```sh
# kill any lingering processes
pkill -f "target/release/iota-indexer.*query_parity\|target/release/iota-indexer.*migration_parity"

# drop test databases (the scripts already do this on success; this is for crashed runs)
psql postgres://postgres:postgrespw@localhost:5432 -c "
  DROP DATABASE IF EXISTS migration_parity_a WITH (FORCE);
  DROP DATABASE IF EXISTS migration_parity_b WITH (FORCE);
  DROP DATABASE IF EXISTS query_parity_old WITH (FORCE);
  DROP DATABASE IF EXISTS query_parity_new WITH (FORCE);
  DROP DATABASE IF EXISTS pruning_test WITH (FORCE);
  DROP DATABASE IF EXISTS pruning_after_migration WITH (FORCE);
"

# stop docker
cd dev-tools/pg-services-local && docker compose down
```


## Gotchas

- **Localnet age**: the localnet's gRPC server can degrade after a couple of
  hours, causing "missing output object key" errors at low checkpoints even
  with pruning fully disabled. If you see this, `docker compose down
  local-network && docker compose up -d local-network` and re-run the workload.
- **Watermark catch-up oscillation**: the parity scripts use a catch-up loop
  because `--stop-at-checkpoint` may settle a few cps short of the target due
  to an in-flight commit-batching race. The loop iterates until both DBs land
  at the same checkpoint (typically 5–20 attempts).
- **Pruning test sync depth**: pruning tests need the indexer to advance
  several epochs past the seed for the pruner to fire. Defaults are tuned for
  the localnet's ~120s epoch; bump `--dwell-seconds` if you tune those.
- **Pre-built binaries**: `--reuse-worktrees --skip-rebuild` on every test
  except the first speeds up iteration significantly.
