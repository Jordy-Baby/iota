#!/usr/bin/env python3
"""Pruning-after-migration test for the backward-history consistency model
(test plan #4).

Verifies that pruning correctly resumes from the migration-set watermark
without off-by-one row drops:

  1. OLD indexer syncs a DB from genesis to cp N (writes objects_snapshot +
     objects_history; no checkpointed_objects / objects_backward_history).
  2. NEW indexer is launched against the same DB with pruning enabled. On
     startup Diesel applies the new migrations, which seed the
     `objects_backward_history` watermark with `min_available_cp = N + 1`
     (i.e. "no backward coverage yet"). Ingestion then resumes from cp N+1,
     writing new rows; the background pruner ticks every 5s and is allowed to
     run by PRUNING_DELAY_MS (5s in our QA build).
  3. After a dwell, assert:
       - `min_available_cp` for `objects_backward_history` advanced PAST the
         migration-set seed (== pruning fired after migration).
       - All rows in `objects_backward_history` have
         `superseded_at_checkpoint >= min_available_cp` (no rows below floor).
       - A row exists exactly at `superseded_at_checkpoint = min_available_cp`
         (no off-by-one dropping the boundary row).
       - GraphQL `availableRange.first.sequenceNumber == min_available_cp`.
"""

import argparse
import dataclasses
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Optional
from urllib.parse import urlparse


DB_NAME = "pruning_after_migration"

DEFAULT_BASE_BRANCH = "develop"
DEFAULT_BACKWARD_BRANCH = "infra/feat/backward-history-consistent-views"
DEFAULT_FLAG_BRANCH = "sc-platform/consistent-views-qa"

DEFAULT_TABLES_TO_PRUNE = [
    "objects_history",
    "objects_backward_history",
]


@dataclasses.dataclass
class Args:
    pg_url: str
    remote_store_url: str
    sync_to_cp: int
    dwell_seconds: int
    epochs_to_keep: int
    tables_to_prune: list[str]
    base_branch: str
    backward_branch: str
    flag_branch: str
    repo_root: Path
    worktree_dir: Path
    old_indexer_binary: Optional[Path]
    new_indexer_binary: Optional[Path]
    backward_graphql_binary: Optional[Path]
    reuse_worktrees: bool
    skip_rebuild: bool
    keep_db: bool
    metrics_port: int
    graphql_port: int
    log_dir: Path
    db_name: str


def parse_args() -> Args:
    here = Path(__file__).resolve()
    default_repo = here.parents[2]
    default_worktree_dir = default_repo.parent / "migration_parity_worktrees"

    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--pg-url", required=True)
    p.add_argument("--remote-store-url", required=True)
    p.add_argument("--sync-to-cp", type=int, default=3000,
                   help="Where the OLD indexer should stop before the NEW one resumes. "
                        "Must be high enough that the migration seed sits below "
                        "current_epoch - retention so pruning can advance past it.")
    p.add_argument("--dwell-seconds", type=int, default=180,
                   help="How long to let the NEW indexer run with pruning enabled. "
                        "Needs to cover at least (retention + 1) * epoch_duration so "
                        "the pruner can advance min_available_cp past the migration seed.")
    p.add_argument("--epochs-to-keep", type=int, default=1)
    p.add_argument("--tables-to-prune", nargs="+", default=DEFAULT_TABLES_TO_PRUNE)
    p.add_argument("--base-branch", default=DEFAULT_BASE_BRANCH)
    p.add_argument("--backward-branch", default=DEFAULT_BACKWARD_BRANCH)
    p.add_argument("--flag-branch", default=DEFAULT_FLAG_BRANCH)
    p.add_argument("--repo-root", type=Path, default=default_repo)
    p.add_argument("--worktree-dir", type=Path, default=default_worktree_dir)
    p.add_argument("--old-indexer-binary", type=Path, default=None)
    p.add_argument("--new-indexer-binary", type=Path, default=None)
    p.add_argument("--backward-graphql-binary", type=Path, default=None)
    p.add_argument("--reuse-worktrees", action="store_true")
    p.add_argument("--skip-rebuild", action="store_true")
    p.add_argument("--keep-db", action="store_true")
    p.add_argument("--metrics-port", type=int, default=19198)
    p.add_argument("--graphql-port", type=int, default=18031)
    p.add_argument("--log-dir", type=Path,
                   default=Path(tempfile.gettempdir()) / "pruning_after_migration_logs")
    p.add_argument("--db-name", default=DB_NAME)
    a = p.parse_args()
    return Args(**vars(a))


def log(msg: str) -> None:
    print(f"[pruning-after-mig] {msg}", flush=True)


# ---------------------------------------------------------------------------
# Postgres / worktree plumbing (same shape as the parity scripts)
# ---------------------------------------------------------------------------


def db_url(base: str, dbname: str) -> str:
    return f"{base.rstrip('/')}/{dbname}"


def run_psql(pg_url: str, sql: str, dbname: str = "postgres") -> None:
    subprocess.run(
        ["psql", db_url(pg_url, dbname), "-v", "ON_ERROR_STOP=1", "-X", "-q", "-c", sql],
        check=True,
    )


def query_psql(pg_url: str, sql: str, dbname: str) -> str:
    r = subprocess.run(
        ["psql", db_url(pg_url, dbname), "-v", "ON_ERROR_STOP=1", "-X", "-A", "-t", "-c", sql],
        check=True, capture_output=True, text=True,
    )
    return r.stdout.strip()


def reset_db(pg_url: str, name: str) -> None:
    log(f"resetting database {name}")
    run_psql(pg_url, f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE);')
    run_psql(pg_url, f'CREATE DATABASE "{name}";')


def sanitize_branch(name: str) -> str:
    return name.replace("/", "_").replace(":", "_")


def ensure_worktree(repo_root: Path, worktree_dir: Path, branch: str,
                    flag_branch: str, reuse: bool) -> Path:
    worktree_dir.mkdir(parents=True, exist_ok=True)
    wt_path = worktree_dir / sanitize_branch(branch)
    if wt_path.exists() and reuse:
        log(f"reusing worktree at {wt_path}")
        return wt_path
    for ref in (branch, flag_branch):
        if subprocess.run(["git", "rev-parse", "--verify", "--quiet", ref],
                          cwd=repo_root, capture_output=True).returncode != 0:
            sys.exit(f"local ref {ref!r} not found in {repo_root}")
    if not wt_path.exists():
        subprocess.run([
            "git", "worktree", "add",
            "-b", f"migration-parity/{sanitize_branch(branch)}",
            str(wt_path), branch,
        ], cwd=repo_root, check=True)
    subprocess.run(["git", "reset", "--hard", branch], cwd=wt_path, check=True)
    flag_sha = subprocess.run(["git", "rev-parse", flag_branch],
                              cwd=wt_path, check=True, capture_output=True, text=True).stdout.strip()
    if subprocess.run(["git", "merge-base", "--is-ancestor", flag_sha, "HEAD"],
                      cwd=wt_path).returncode != 0:
        try:
            subprocess.run(["git", "cherry-pick", flag_sha], cwd=wt_path, check=True)
        except subprocess.CalledProcessError as e:
            subprocess.run(["git", "cherry-pick", "--abort"], cwd=wt_path, check=False)
            sys.exit(f"cherry-pick {flag_sha[:10]} failed: {e}")
    return wt_path


def build_in_worktree(worktree: Path, packages: list[str], expected: list[str],
                      skip_if_exists: bool) -> None:
    if skip_if_exists and all((worktree / "target" / "release" / b).exists() for b in expected):
        log(f"reusing existing binaries in {worktree}")
        return
    cmd = ["cargo", "build", "--release"]
    for p in packages:
        cmd += ["-p", p]
    log(f"building {packages} in {worktree}")
    subprocess.run(cmd, cwd=worktree, check=True)


def run_indexer(binary: Path, db_full: str, stop_at: Optional[int], remote_store_url: str,
                metrics_port: int, log_path: Path, tag: str,
                pruning_config: Optional[Path] = None,
                dwell_seconds: int = 0) -> None:
    cmd = [
        str(binary),
        f"--database-url={db_full}",
        f"--metrics-address=0.0.0.0:{metrics_port}",
        "indexer",
        f"--remote-store-url={remote_store_url}",
    ]
    if dwell_seconds <= 0 and stop_at is not None:
        cmd.append(f"--stop-at-checkpoint={stop_at}")
    if pruning_config is not None:
        cmd.append(f"--pruning-config-path={pruning_config}")
    log(f"[{tag}] starting indexer "
        + (f"with {dwell_seconds}s dwell" if dwell_seconds > 0 else f"to cp {stop_at}")
        + (f" (pruning={pruning_config.name})" if pruning_config else ""))
    log_path.parent.mkdir(parents=True, exist_ok=True)
    lf = log_path.open("w")
    if dwell_seconds <= 0:
        rc = subprocess.run(cmd, stdout=lf, stderr=subprocess.STDOUT).returncode
        lf.close()
        if rc != 0:
            sys.exit(f"[{tag}] indexer exited non-zero ({rc}); see {log_path}")
        return
    proc = subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT)
    try:
        for _ in range(dwell_seconds):
            if proc.poll() is not None:
                lf.close()
                sys.exit(f"[{tag}] indexer exited early during dwell; see {log_path}")
            time.sleep(1)
    finally:
        log(f"[{tag}] dwell complete; sending SIGTERM")
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            log(f"[{tag}] SIGTERM didn't exit in 60s, sending SIGKILL")
            proc.kill()
            proc.wait()
        lf.close()


def get_watermark(pg_url: str, dbname: str, entity: str = "checkpoints") -> Optional[int]:
    val = query_psql(
        pg_url,
        f"SELECT COALESCE(MAX(max_committed_cp)::text, 'NULL') FROM watermarks WHERE entity = '{entity}'",
        dbname,
    )
    if val == "NULL":
        return None
    try:
        return int(val) if val else None
    except ValueError:
        sys.exit(f"unexpected watermark value: {val!r}")


def get_min_available_cp(pg_url: str, dbname: str, entity: str) -> Optional[int]:
    val = query_psql(
        pg_url,
        f"SELECT COALESCE(min_available_cp::text, 'NULL') FROM watermarks WHERE entity = '{entity}'",
        dbname,
    )
    if val in ("NULL", ""):
        return None
    return int(val)


# ---------------------------------------------------------------------------
# GraphQL plumbing
# ---------------------------------------------------------------------------


def start_graphql_rpc(binary: Path, db_full: str, node_rpc_url: str, port: int,
                      log_path: Path, tag: str) -> subprocess.Popen:
    cmd = [
        str(binary),
        "start-server",
        f"--db-url={db_full}",
        "--host=127.0.0.1",
        f"--port={port}",
        "--prom-host=127.0.0.1",
        f"--prom-port={port + 1000}",
        f"--node-rpc-url={node_rpc_url}",
    ]
    log(f"[{tag}] starting graphql-rpc on port {port}")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    lf = log_path.open("w")
    proc = subprocess.Popen(cmd, stdout=lf, stderr=subprocess.STDOUT)
    proc._lf = lf  # type: ignore[attr-defined]
    url = f"http://127.0.0.1:{port}/graphql"
    for _ in range(60):
        try:
            req = urllib.request.Request(url, b'{"query":"{ chainIdentifier }"}',
                                         {"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=2) as r:
                if r.status == 200:
                    return proc
        except Exception:
            pass
        if proc.poll() is not None:
            sys.exit(f"[{tag}] graphql-rpc exited early; see {log_path}")
        time.sleep(1)
    proc.terminate()
    sys.exit(f"[{tag}] graphql-rpc never became ready on port {port}; see {log_path}")


def stop_graphql_rpc(proc: subprocess.Popen) -> None:
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    lf = getattr(proc, "_lf", None)
    if lf is not None:
        lf.close()


def graphql_post(url: str, query: str) -> dict:
    body = json.dumps({"query": query}).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read())


# ---------------------------------------------------------------------------
# Pruning config
# ---------------------------------------------------------------------------


HUGE_EPOCHS = 1_000_000


def write_pruning_config(args: Args, dest: Path) -> Path:
    lines = [
        "# Generated by pruning_after_migration_test.py — do not edit manually.",
        f"epochs_to_keep = {HUGE_EPOCHS}",
        "",
        "[overrides]",
    ]
    for t in args.tables_to_prune:
        lines.append(f"{t} = {args.epochs_to_keep}")
    body = "\n".join(lines) + "\n"
    dest.write_text(body)
    log(f"wrote pruning config to {dest}:\n{body}")
    return dest


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def resolve_defaults(args: Args) -> None:
    base_wt = args.worktree_dir / sanitize_branch(args.base_branch)
    new_wt = args.worktree_dir / sanitize_branch(args.backward_branch)
    if args.old_indexer_binary is None:
        args.old_indexer_binary = base_wt / "target" / "release" / "iota-indexer"
    if args.new_indexer_binary is None:
        args.new_indexer_binary = new_wt / "target" / "release" / "iota-indexer"
    if args.backward_graphql_binary is None:
        args.backward_graphql_binary = new_wt / "target" / "release" / "iota-graphql-rpc"


def main() -> int:
    args = parse_args()

    parsed = urlparse(args.pg_url)
    if parsed.path and parsed.path != "/":
        sys.exit(f"--pg-url must not include a database name, got path={parsed.path!r}")

    log(f"sync_to_cp={args.sync_to_cp} dwell={args.dwell_seconds}s "
        f"epochs_to_keep={args.epochs_to_keep} tables={args.tables_to_prune}")

    # Phase 0: prepare binaries.
    log("phase 0: prepare binaries")
    ensure_worktree(args.repo_root, args.worktree_dir,
                    args.base_branch, args.flag_branch, reuse=args.reuse_worktrees)
    build_in_worktree(
        args.worktree_dir / sanitize_branch(args.base_branch),
        ["iota-indexer", "iota-node"],
        ["iota-indexer"],
        skip_if_exists=args.skip_rebuild,
    )
    ensure_worktree(args.repo_root, args.worktree_dir,
                    args.backward_branch, args.flag_branch, reuse=args.reuse_worktrees)
    build_in_worktree(
        args.worktree_dir / sanitize_branch(args.backward_branch),
        ["iota-indexer", "iota-node", "iota-graphql-rpc"],
        ["iota-indexer", "iota-graphql-rpc"],
        skip_if_exists=args.skip_rebuild,
    )
    resolve_defaults(args)
    for b in (args.old_indexer_binary, args.new_indexer_binary, args.backward_graphql_binary):
        if not b.is_file():
            sys.exit(f"binary not found: {b}")

    # Phase 1: write pruning config.
    args.log_dir.mkdir(parents=True, exist_ok=True)
    pruning_toml = write_pruning_config(args, args.log_dir / "pruning.toml")

    # Phase 2: OLD indexer syncs to sync_to_cp.
    log(f"phase 2: OLD indexer syncs to cp {args.sync_to_cp}")
    reset_db(args.pg_url, args.db_name)
    run_indexer(args.old_indexer_binary, db_url(args.pg_url, args.db_name),
                stop_at=args.sync_to_cp, remote_store_url=args.remote_store_url,
                metrics_port=args.metrics_port,
                log_path=args.log_dir / "old_sync.log", tag="OLD")
    settled = get_watermark(args.pg_url, args.db_name)
    log(f"OLD indexer settled at cp {settled}")
    # The migration's seed will be settled+1. Capture for later comparison.

    # Phase 3: NEW indexer with pruning enabled — runs migration on startup AND
    # ingests + prunes during the dwell. Migrations seed
    # objects_backward_history.min_available_cp = settled + 1; from then on,
    # the pruner advances it as new rows arrive.
    log(f"phase 3: NEW indexer with pruning, {args.dwell_seconds}s dwell")
    run_indexer(args.new_indexer_binary, db_url(args.pg_url, args.db_name),
                stop_at=None, remote_store_url=args.remote_store_url,
                metrics_port=args.metrics_port,
                log_path=args.log_dir / "new_dwell.log", tag="NEW-prune",
                pruning_config=pruning_toml, dwell_seconds=args.dwell_seconds)
    final_cp = get_watermark(args.pg_url, args.db_name)
    log(f"after dwell: checkpoints watermark = {final_cp}")

    # Phase 4: verify pruning resumed properly.
    log("phase 4: verify pruning resumed from migration seed")
    seed_floor = settled + 1  # what the migration would have set
    bw_min = get_min_available_cp(args.pg_url, args.db_name, "objects_backward_history")
    if bw_min is None:
        sys.exit("objects_backward_history has no watermark entry — migration didn't run?")
    log(f"  objects_backward_history.min_available_cp = {bw_min} (migration seed was ~{seed_floor})")
    if bw_min <= seed_floor:
        sys.exit(f"pruning watermark {bw_min} did not advance past the migration seed "
                 f"({seed_floor}); pruner didn't fire after migration")

    # No rows below the floor.
    row_check = query_psql(
        args.pg_url,
        "SELECT COUNT(*), COALESCE(MIN(superseded_at_checkpoint)::text, 'NULL'), "
        "COALESCE(MAX(superseded_at_checkpoint)::text, 'NULL') "
        "FROM objects_backward_history",
        args.db_name,
    )
    count_s, min_s, max_s = row_check.split("|")
    count = int(count_s)
    min_v = None if min_s == "NULL" else int(min_s)
    max_v = None if max_s == "NULL" else int(max_s)
    log(f"  objects_backward_history rows: count={count} min={min_v} max={max_v}")
    if count == 0:
        sys.exit("objects_backward_history is empty — over-pruned, or NEW indexer didn't ingest")
    if min_v is None or min_v < bw_min:
        sys.exit(f"row below floor: min({min_v}) < min_available_cp({bw_min})")

    # Boundary row exists exactly at bw_min (no off-by-one dropping it).
    boundary = query_psql(
        args.pg_url,
        f"SELECT COUNT(*) FROM objects_backward_history "
        f"WHERE superseded_at_checkpoint = {bw_min}",
        args.db_name,
    )
    log(f"  rows at exactly superseded_at_checkpoint={bw_min}: {boundary}")
    if int(boundary) == 0:
        sys.exit(f"OFF-BY-ONE: no row at the boundary cp {bw_min}; pruner over-pruned")
    log("  boundary row present — no off-by-one")

    # Phase 5: verify GraphQL availableRange tracks the post-pruning watermark.
    log("phase 5: verify graphql availableRange")
    gql = start_graphql_rpc(args.backward_graphql_binary,
                            db_url(args.pg_url, args.db_name),
                            args.remote_store_url, args.graphql_port,
                            args.log_dir / "graphql.log", tag="graphql")
    try:
        payload = graphql_post(
            f"http://127.0.0.1:{args.graphql_port}/graphql",
            "{ availableRange { first { sequenceNumber } last { sequenceNumber } } }",
        )
    finally:
        stop_graphql_rpc(gql)

    if "errors" in payload:
        sys.exit(f"availableRange returned errors: {payload['errors']}")
    rng = (payload.get("data") or {}).get("availableRange") or {}
    first = (rng.get("first") or {}).get("sequenceNumber")
    last = (rng.get("last") or {}).get("sequenceNumber")
    log(f"availableRange: first={first} last={last}")
    if first is None or int(first) < bw_min:
        sys.exit(f"availableRange.first={first} below pruning watermark ({bw_min})")
    log(f"OK: availableRange.first={first} >= pruning watermark {bw_min}")

    if not args.keep_db:
        log(f"cleanup: dropping {args.db_name}")
        run_psql(args.pg_url, f'DROP DATABASE IF EXISTS "{args.db_name}" WITH (FORCE);')
    return 0


if __name__ == "__main__":
    sys.exit(main())
