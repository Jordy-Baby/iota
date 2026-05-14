#!/usr/bin/env python3
"""Pruning test for the backward-history consistency model (test plan #3).

Sync the NEW indexer with pruning enabled for `objects_backward_history` (and
related tables), then verify:

  1. Rows are actually removed from `objects_backward_history` — its
     `MIN(superseded_at_checkpoint)` is >= the pruning watermark.
  2. Watermarks are updated — `min_available_cp` for the pruned entity is
     greater than 0 (i.e. pruning has advanced past genesis).
  3. GraphQL `availableRange.first.sequenceNumber` reflects the new pruning
     floor (not just the BACKWARD_HISTORY_MAX_LOOKBACK = 900 lag cap).

Prerequisites:
  - Running postgres reachable at --pg-url (URL without db name).
  - Running localnet reachable at --remote-store-url.
  - Local branches `backward-branch` + `flag-branch` present (for the NEW
    indexer + backward graphql-rpc).
  - Workload manifest (optional, used only to suggest a target cp; otherwise
    sync to whatever the localnet currently has).
"""

import argparse
import dataclasses
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Optional
from urllib.parse import urlparse


DB_NAME = "pruning_test"

DEFAULT_BACKWARD_BRANCH = "infra/feat/backward-history-consistent-views"
DEFAULT_FLAG_BRANCH = "sc-platform/consistent-views-qa"

DEFAULT_TABLES_TO_PRUNE = [
    "objects_history",
    "objects_backward_history",
]


@dataclasses.dataclass
class Args:
    manifest: Optional[Path]
    pg_url: str
    remote_store_url: str
    backward_branch: str
    flag_branch: str
    repo_root: Path
    worktree_dir: Path
    new_indexer_binary: Optional[Path]
    backward_graphql_binary: Optional[Path]
    dwell_seconds: int
    epochs_to_keep: int
    tables_to_prune: list[str]
    reuse_worktrees: bool
    skip_rebuild: bool
    reuse_db: bool
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
    p.add_argument("--manifest", type=Path, default=None,
                   help="Workload manifest; used only to suggest target_cp if not provided.")
    p.add_argument("--pg-url", required=True)
    p.add_argument("--remote-store-url", required=True)
    p.add_argument("--backward-branch", default=DEFAULT_BACKWARD_BRANCH)
    p.add_argument("--flag-branch", default=DEFAULT_FLAG_BRANCH)
    p.add_argument("--repo-root", type=Path, default=default_repo)
    p.add_argument("--worktree-dir", type=Path, default=default_worktree_dir)
    p.add_argument("--new-indexer-binary", type=Path, default=None)
    p.add_argument("--backward-graphql-binary", type=Path, default=None)
    p.add_argument("--dwell-seconds", type=int, default=30,
                   help="How long to let the indexer run (so the 5-sec pruner can tick).")
    p.add_argument("--epochs-to-keep", type=int, default=2,
                   help="Pruning retention (epochs) for the prunable tables.")
    p.add_argument("--tables-to-prune", nargs="+", default=DEFAULT_TABLES_TO_PRUNE)
    p.add_argument("--reuse-worktrees", action="store_true")
    p.add_argument("--skip-rebuild", action="store_true")
    p.add_argument("--reuse-db", action="store_true")
    p.add_argument("--keep-db", action="store_true")
    p.add_argument("--metrics-port", type=int, default=19199)
    p.add_argument("--graphql-port", type=int, default=18021)
    p.add_argument("--log-dir", type=Path,
                   default=Path(tempfile.gettempdir()) / "pruning_test_logs")
    p.add_argument("--db-name", default=DB_NAME)
    a = p.parse_args()
    return Args(**vars(a))


def log(msg: str) -> None:
    print(f"[pruning-test] {msg}", flush=True)


# ---------------------------------------------------------------------------
# Postgres + worktree plumbing (mirrors helpers in the parity scripts)
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


def run_indexer(binary: Path, db_full: str, stop_at: int, remote_store_url: str,
                metrics_port: int, log_path: Path, tag: str,
                pruning_config: Optional[Path] = None,
                dwell_seconds: int = 0) -> None:
    """Run the indexer.

    If `dwell_seconds > 0`, the indexer is launched WITHOUT --stop-at-checkpoint,
    sleeps for `dwell_seconds`, then is SIGTERMed. The pruner (5s interval task)
    needs the indexer to stay alive long enough to actually tick multiple times.
    """
    cmd = [
        str(binary),
        f"--database-url={db_full}",
        f"--metrics-address=0.0.0.0:{metrics_port}",
        "indexer",
        f"--remote-store-url={remote_store_url}",
    ]
    if dwell_seconds <= 0:
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
    # Dwell mode: spawn, sleep, SIGTERM.
    import signal
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


def get_watermark(pg_url: str, dbname: str, entity: str = "checkpoints") -> int:
    val = query_psql(
        pg_url,
        f"SELECT COALESCE(MAX(max_committed_cp), -1) FROM watermarks WHERE entity = '{entity}'",
        dbname,
    )
    try:
        return int(val) if val else -1
    except ValueError:
        sys.exit(f"unexpected watermark value: {val!r}")


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


def graphql_post(url: str, query: str, variables: Optional[dict] = None) -> dict:
    body = json.dumps({"query": query, "variables": variables or {}}).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.loads(r.read())


# ---------------------------------------------------------------------------
# Pruning config
# ---------------------------------------------------------------------------


HUGE_EPOCHS = 1_000_000


def write_pruning_config(args: Args, dest: Path) -> Path:
    """Write a TOML retention config that prunes only `tables_to_prune`."""
    lines = [
        "# Generated by pruning_test.py — do not edit manually.",
        f"epochs_to_keep = {HUGE_EPOCHS}  # default: effectively disabled",
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
# Assertions
# ---------------------------------------------------------------------------


def assert_pruning_happened(pg_url: str, dbname: str, tables: list[str]) -> dict[str, int]:
    """Return the min_available_cp per table; assert > 0 for each."""
    log("checking pruning watermarks...")
    out = {}
    for t in tables:
        sql = (
            f"SELECT COALESCE(min_available_cp, -1) FROM watermarks WHERE entity = '{t}'"
        )
        val = query_psql(pg_url, sql, dbname)
        try:
            cp = int(val) if val else -1
        except ValueError:
            sys.exit(f"unexpected watermark for {t}: {val!r}")
        log(f"  {t}: min_available_cp = {cp}")
        out[t] = cp
        if cp <= 0:
            sys.exit(f"pruning didn't advance min_available_cp for {t} (still {cp})")
    return out


def assert_rows_pruned(pg_url: str, dbname: str, table: str, column: str,
                       min_available_cp: int) -> tuple[int, Optional[int]]:
    """Return (count, min_value); assert min_value >= min_available_cp."""
    sql = f"SELECT COUNT(*), COALESCE(MIN({column})::text, 'NULL') FROM {table}"
    val = query_psql(pg_url, sql, dbname)
    count_s, min_s = val.split("|")
    count = int(count_s)
    min_v = None if min_s == "NULL" else int(min_s)
    log(f"  {table}: count={count} min({column})={min_v}")
    if count == 0:
        sys.exit(f"{table} is entirely empty — likely over-pruned or unfilled")
    if min_v is None:
        sys.exit(f"{table} has no rows with non-null {column}")
    if min_v < min_available_cp:
        sys.exit(f"{table} has rows below pruning watermark "
                 f"(min({column})={min_v} < min_available_cp={min_available_cp})")
    return count, min_v


def query_available_range(graphql_url: str) -> dict:
    return graphql_post(
        graphql_url,
        "{ availableRange { first { sequenceNumber } last { sequenceNumber } } }",
    )


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def resolve_defaults(args: Args) -> None:
    wt = args.worktree_dir / sanitize_branch(args.backward_branch)
    if args.new_indexer_binary is None:
        args.new_indexer_binary = wt / "target" / "release" / "iota-indexer"
    if args.backward_graphql_binary is None:
        args.backward_graphql_binary = wt / "target" / "release" / "iota-graphql-rpc"


def main() -> int:
    args = parse_args()

    parsed = urlparse(args.pg_url)
    if parsed.path and parsed.path != "/":
        sys.exit(f"--pg-url must not include a database name, got path={parsed.path!r}")

    log(f"dwell={args.dwell_seconds}s; epochs_to_keep={args.epochs_to_keep}; "
        f"tables_to_prune={args.tables_to_prune}")

    # Phase 0: prepare binaries.
    log("phase 0: prepare binaries")
    if not args.reuse_db:
        ensure_worktree(args.repo_root, args.worktree_dir,
                        args.backward_branch, args.flag_branch, reuse=args.reuse_worktrees)
        build_in_worktree(
            args.worktree_dir / sanitize_branch(args.backward_branch),
            ["iota-indexer", "iota-node", "iota-graphql-rpc"],
            ["iota-indexer", "iota-graphql-rpc"],
            skip_if_exists=args.skip_rebuild,
        )
    else:
        ensure_worktree(args.repo_root, args.worktree_dir,
                        args.backward_branch, args.flag_branch, reuse=True)
    resolve_defaults(args)
    for b in (args.new_indexer_binary, args.backward_graphql_binary):
        if not b.is_file():
            sys.exit(f"binary not found: {b}")

    # Phase 1: write pruning config.
    log("phase 1: write pruning config")
    args.log_dir.mkdir(parents=True, exist_ok=True)
    pruning_toml = write_pruning_config(args, args.log_dir / "pruning.toml")

    # Phase 2: run NEW indexer with pruning enabled for `dwell_seconds`.
    if not args.reuse_db:
        log(f"phase 2: run NEW indexer for {args.dwell_seconds}s with pruning enabled")
        reset_db(args.pg_url, args.db_name)
        run_indexer(args.new_indexer_binary, db_url(args.pg_url, args.db_name),
                    stop_at=0, remote_store_url=args.remote_store_url,
                    metrics_port=args.metrics_port,
                    log_path=args.log_dir / "indexer.log", tag="NEW-prune",
                    pruning_config=pruning_toml, dwell_seconds=args.dwell_seconds)
        actual = get_watermark(args.pg_url, args.db_name)
        log(f"after dwell: checkpoints watermark = {actual}")
    else:
        log(f"phase 2: reusing existing {args.db_name}")
        actual = get_watermark(args.pg_url, args.db_name)
        log(f"reused db watermark: {actual}")

    # Phase 3: verify pruning happened at the DB layer.
    log("phase 3: verify DB state")
    watermarks = assert_pruning_happened(args.pg_url, args.db_name, args.tables_to_prune)
    # Row-level check for objects_backward_history specifically.
    if "objects_backward_history" in args.tables_to_prune:
        assert_rows_pruned(
            args.pg_url, args.db_name,
            "objects_backward_history", "superseded_at_checkpoint",
            watermarks["objects_backward_history"],
        )

    # Phase 4: verify GraphQL availableRange reflects the pruning floor.
    log("phase 4: verify graphql availableRange tracks the pruning watermark")
    gql = start_graphql_rpc(args.backward_graphql_binary,
                            db_url(args.pg_url, args.db_name),
                            args.remote_store_url, args.graphql_port,
                            args.log_dir / "graphql.log", tag="graphql")
    try:
        payload = query_available_range(f"http://127.0.0.1:{args.graphql_port}/graphql")
    finally:
        stop_graphql_rpc(gql)

    if "errors" in payload:
        sys.exit(f"availableRange returned errors: {payload['errors']}")
    rng = (payload.get("data") or {}).get("availableRange") or {}
    first = (rng.get("first") or {}).get("sequenceNumber")
    last = (rng.get("last") or {}).get("sequenceNumber")
    log(f"availableRange: first={first} last={last}")

    expected_first = watermarks["objects_backward_history"]
    if first is None:
        sys.exit("availableRange.first is null — expected the pruning floor")
    if int(first) < expected_first:
        sys.exit(f"availableRange.first={first} is below pruning watermark "
                 f"({expected_first}); GraphQL didn't honour the floor")
    log(f"OK: availableRange.first={first} >= pruning watermark {expected_first}")

    if not args.keep_db and not args.reuse_db:
        log(f"cleanup: dropping {args.db_name}")
        run_psql(args.pg_url, f'DROP DATABASE IF EXISTS "{args.db_name}" WITH (FORCE);')
    return 0


if __name__ == "__main__":
    sys.exit(main())
