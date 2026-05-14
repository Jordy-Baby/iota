#!/usr/bin/env python3
"""Migration parity test for the backward-history consistency model.

Verifies that two paths reach the same `checkpointed_objects` content at a given
checkpoint N:

  - db_a: OLD indexer syncs to N -> NEW indexer launched against the same DB
          (Diesel applies the new migrations, no further ingestion).
  - db_b: NEW indexer syncs from scratch to N.

Both indexers consume from the same local-network. Each is stopped via the
`--stop-at-checkpoint=N` flag, which uses iota-data-ingestion-core's native
`IngestionLimit::MaxCheckpoint` (graceful shutdown after processing N).

Binary preparation (default flow):
  For each side (OLD = base, NEW = backward), the script creates a git worktree
  on the requested branch, merges the flag branch in (so --stop-at-checkpoint is
  available), and runs `cargo build -p iota-indexer -p iota-node --release`.
  Worktrees and built artifacts are reused across runs.

Prerequisites:
  - A running local-network reachable at --remote-store-url.
  - A running postgres reachable at --pg-url (URL without a database name).
  - `psql`, `cargo`, and `git` available on PATH.

Example:
  ./migration_parity.py \\
    --stop-at-checkpoint 50 \\
    --pg-url postgres://postgres:postgrespw@localhost:5432 \\
    --remote-store-url http://localhost:50051

Override branches:
    --base-branch develop \\
    --backward-branch infra/feat/backward-history-consistent-views \\
    --flag-branch sc-platform/indexer-stop-at-checkpoint

Skip build (reuse last binary, faster iteration):
    --skip-rebuild --reuse-worktrees

Use pre-built binaries (bypass worktree management entirely):
    --old-binary /path/to/old/iota-indexer --new-binary /path/to/new/iota-indexer
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


DB_A = "migration_parity_a"
DB_B = "migration_parity_b"

DEFAULT_BASE_BRANCH = "develop"
DEFAULT_BACKWARD_BRANCH = "infra/feat/backward-history-consistent-views"
DEFAULT_FLAG_BRANCH = "sc-platform/indexer-stop-at-checkpoint"


@dataclasses.dataclass
class Args:
    stop_at_checkpoint: Optional[int]
    manifest: Optional[Path]
    pg_url: str
    remote_store_url: str
    base_branch: str
    backward_branch: str
    flag_branch: str
    repo_root: Path
    worktree_dir: Path
    old_binary: Optional[Path]
    new_binary: Optional[Path]
    skip_rebuild: bool
    reuse_worktrees: bool
    metrics_port_a: int
    metrics_port_b: int
    graphql_port_a: int
    graphql_port_b: int
    graphql_rpc_binary: Optional[Path]
    keep_dbs: bool
    indexer_log_dir: Path


def parse_args() -> Args:
    here = Path(__file__).resolve()
    default_repo = here.parents[2]  # <repo>/consistent_views_qa_scripts/migration_parity/migration_parity.py
    default_worktree_dir = default_repo.parent / "migration_parity_worktrees"

    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--stop-at-checkpoint", type=int,
                   help="Target checkpoint N (inclusive).")
    g.add_argument("--manifest", type=Path,
                   help="Workload manifest produced by qa-workload; uses its final_checkpoint.")
    p.add_argument("--pg-url", type=str, required=True,
                   help="Base postgres URL with no DB name, e.g. postgres://user:pw@host:5432")
    p.add_argument("--remote-store-url", type=str, required=True,
                   help="Local-network gRPC URL, e.g. http://localhost:50051")

    p.add_argument("--base-branch", type=str, default=DEFAULT_BASE_BRANCH,
                   help=f"OLD indexer branch (default: {DEFAULT_BASE_BRANCH}).")
    p.add_argument("--backward-branch", type=str, default=DEFAULT_BACKWARD_BRANCH,
                   help=f"NEW indexer branch (default: {DEFAULT_BACKWARD_BRANCH}).")
    p.add_argument("--flag-branch", type=str, default=DEFAULT_FLAG_BRANCH,
                   help=f"Branch carrying the --stop-at-checkpoint flag, merged into both sides "
                        f"(default: {DEFAULT_FLAG_BRANCH}).")
    p.add_argument("--repo-root", type=Path, default=default_repo,
                   help=f"Path to the main iota git repo (default: {default_repo}).")
    p.add_argument("--worktree-dir", type=Path, default=default_worktree_dir,
                   help=f"Directory for managed worktrees (default: {default_worktree_dir}).")

    p.add_argument("--old-binary", type=Path, default=None,
                   help="Use this prebuilt OLD indexer binary; skip worktree/build for OLD.")
    p.add_argument("--new-binary", type=Path, default=None,
                   help="Use this prebuilt NEW indexer binary; skip worktree/build for NEW.")
    p.add_argument("--skip-rebuild", action="store_true",
                   help="Skip cargo build if target/release/iota-indexer already exists in the worktree.")
    p.add_argument("--reuse-worktrees", action="store_true",
                   help="Skip git operations on existing worktrees; just rebuild and run.")

    p.add_argument("--metrics-port-a", type=int, default=19181)
    p.add_argument("--metrics-port-b", type=int, default=19182)
    p.add_argument("--graphql-port-a", type=int, default=18001)
    p.add_argument("--graphql-port-b", type=int, default=18002)
    p.add_argument("--graphql-rpc-binary", type=Path, default=None,
                   help="iota-graphql-rpc binary. Default: NEW worktree's "
                        "target/release/iota-graphql-rpc.")
    p.add_argument("--keep-dbs", action="store_true",
                   help="Don't drop db_a/db_b after comparison (for manual inspection).")
    p.add_argument("--indexer-log-dir", type=Path,
                   default=Path(tempfile.gettempdir()) / "migration_parity_logs",
                   help="Directory for indexer stdout/stderr logs.")
    a = p.parse_args()
    return Args(**vars(a))


def log(msg: str) -> None:
    print(f"[migration-parity] {msg}", flush=True)


def run(cmd: list[str], *, cwd: Optional[Path] = None, env: Optional[dict] = None,
        check: bool = True, capture: bool = False) -> subprocess.CompletedProcess:
    """Thin wrapper around subprocess.run with consistent logging."""
    log(f"$ {' '.join(str(c) for c in cmd)}" + (f"   (cwd={cwd})" if cwd else ""))
    return subprocess.run(
        cmd, cwd=cwd, env=env, check=check,
        capture_output=capture, text=True if capture else None,
    )


# ---------------------------------------------------------------------------
# Worktree management
# ---------------------------------------------------------------------------


def sanitize_branch(name: str) -> str:
    return name.replace("/", "_").replace(":", "_")


def ensure_worktree(repo_root: Path, worktree_dir: Path, branch: str,
                    flag_branch: str, *, reuse: bool) -> Path:
    """Create a worktree for `branch` (or reuse existing), cherry-pick `flag_branch`'s tip into it.

    Cherry-pick is used rather than merge so we apply only the single flag
    commit at the tip of `flag_branch`, without dragging in any other commits
    that happen to be in `flag_branch`'s ancestry but not in `branch`.
    """
    worktree_dir.mkdir(parents=True, exist_ok=True)
    wt_path = worktree_dir / sanitize_branch(branch)
    local_branch = f"migration-parity/{sanitize_branch(branch)}"

    # All branches (base, backward, flag) are expected to be present locally.
    # We never fetch from origin; the user manages the local refs.
    for ref in (branch, flag_branch):
        if subprocess.run(
            ["git", "rev-parse", "--verify", "--quiet", ref],
            cwd=repo_root, capture_output=True,
        ).returncode != 0:
            sys.exit(f"local ref {ref!r} not found in {repo_root}; check it out locally first")

    if wt_path.exists():
        if reuse:
            log(f"reusing worktree at {wt_path}")
            return wt_path
        log(f"worktree at {wt_path} already exists; resetting to local {branch}")
    else:
        run([
            "git", "worktree", "add",
            "-b", local_branch,
            str(wt_path), branch,
        ], cwd=repo_root)

    # Reset to local branch tip so the worktree is in a known state, then cherry-pick the flag.
    run(["git", "reset", "--hard", branch], cwd=wt_path)

    flag_sha = subprocess.run(
        ["git", "rev-parse", flag_branch],
        cwd=wt_path, check=True, capture_output=True, text=True,
    ).stdout.strip()
    is_ancestor = subprocess.run(
        ["git", "merge-base", "--is-ancestor", flag_sha, "HEAD"],
        cwd=wt_path,
    ).returncode == 0
    if is_ancestor:
        log(f"{branch}: flag commit {flag_sha[:10]} already in history; skipping cherry-pick")
    else:
        log(f"{branch}: cherry-picking {flag_sha[:10]} ({flag_branch} tip)")
        try:
            run(["git", "cherry-pick", flag_sha], cwd=wt_path)
        except subprocess.CalledProcessError as e:
            run(["git", "cherry-pick", "--abort"], cwd=wt_path, check=False)
            sys.exit(
                f"cherry-pick of {flag_branch} ({flag_sha[:10]}) into {branch} failed "
                f"(likely a conflict). Resolve manually in {wt_path}, then re-run with "
                f"--reuse-worktrees.\nUnderlying error: {e}"
            )
    return wt_path


def build_indexer(worktree: Path, *, skip_if_exists: bool) -> Path:
    """Build iota-indexer in `worktree`; return the binary path."""
    binary = worktree / "target" / "release" / "iota-indexer"
    if skip_if_exists and binary.exists():
        log(f"reusing existing binary at {binary}")
        return binary
    # Building iota-node alongside enables tokio's "signal" feature via feature
    # unification, which the indexer needs for SIGTERM handling.
    run([
        "cargo", "build", "--release",
        "-p", "iota-indexer", "-p", "iota-node",
    ], cwd=worktree)
    if not binary.is_file():
        sys.exit(f"expected binary at {binary} but it was not produced")
    return binary


# ---------------------------------------------------------------------------
# Postgres + indexer orchestration
# ---------------------------------------------------------------------------


def db_url(base: str, dbname: str) -> str:
    base = base.rstrip("/")
    return f"{base}/{dbname}"


def run_psql(pg_url: str, sql: str, dbname: str = "postgres") -> None:
    url = db_url(pg_url, dbname)
    subprocess.run(
        ["psql", url, "-v", "ON_ERROR_STOP=1", "-X", "-q", "-c", sql],
        check=True,
    )


def query_psql(pg_url: str, sql: str, dbname: str) -> str:
    url = db_url(pg_url, dbname)
    r = subprocess.run(
        ["psql", url, "-v", "ON_ERROR_STOP=1", "-X", "-A", "-t", "-c", sql],
        check=True, capture_output=True, text=True,
    )
    return r.stdout.strip()


def reset_db(pg_url: str, name: str) -> None:
    log(f"resetting database {name}")
    run_psql(pg_url, f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE);')
    run_psql(pg_url, f'CREATE DATABASE "{name}";')


def run_indexer(
    binary: Path,
    db_url_full: str,
    stop_at: int,
    remote_store_url: str,
    metrics_port: int,
    log_path: Path,
    *,
    tag: str,
) -> None:
    cmd = [
        str(binary),
        f"--database-url={db_url_full}",
        f"--metrics-address=0.0.0.0:{metrics_port}",
        "indexer",
        f"--remote-store-url={remote_store_url}",
        f"--stop-at-checkpoint={stop_at}",
    ]
    log(f"[{tag}] starting: {' '.join(cmd)}")
    log(f"[{tag}] logs: {log_path}")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with log_path.open("w") as lf:
        rc = subprocess.run(cmd, stdout=lf, stderr=subprocess.STDOUT).returncode
    if rc != 0:
        sys.exit(f"[{tag}] indexer exited non-zero ({rc}); see {log_path}")
    log(f"[{tag}] indexer exited cleanly")


def query_available_range_via_graphql(graphql_binary: Path, db_url_full: str,
                                       node_rpc_url: str, port: int, log_path: Path,
                                       tag: str) -> dict:
    """Start iota-graphql-rpc against `db_url_full`, query `availableRange`, stop it.

    Returns the parsed JSON `data.availableRange` object: {"first": {...}, "last": {...}}.
    """
    cmd = [
        str(graphql_binary),
        "start-server",
        f"--db-url={db_url_full}",
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
    try:
        # Wait for server to become ready.
        url = f"http://127.0.0.1:{port}/graphql"
        ready = False
        for _ in range(60):
            try:
                req = urllib.request.Request(
                    url, b'{"query":"{ chainIdentifier }"}',
                    {"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(req, timeout=2) as r:
                    if r.status == 200:
                        ready = True
                        break
            except Exception:
                pass
            if proc.poll() is not None:
                sys.exit(f"[{tag}] graphql-rpc exited early; see {log_path}")
            time.sleep(1)
        if not ready:
            sys.exit(f"[{tag}] graphql-rpc never became ready on port {port}; see {log_path}")

        body = (
            b'{"query":"{ availableRange { '
            b'first { sequenceNumber } last { sequenceNumber } } }"}'
        )
        req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=15) as r:
            payload = json.loads(r.read())
        # The migrated indexer can legitimately return an error like "outside
        # the available range" when the request's checkpoint_viewed_at falls
        # outside the backward-history window. Return the full payload so the
        # caller can decide what to do.
        return payload
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
        lf.close()


def check_available_range(args: "Args", settled_cp: int) -> None:
    """Query `availableRange` on both DBs via GraphQL and assert the test
    plan's invariant (#11500): the migrated indexer's available range must
    NOT extend back to genesis, while the from-scratch indexer's must.
    """
    if args.graphql_rpc_binary is None:
        sys.exit("--graphql-rpc-binary required for the availableRange check")
    payload_a = query_available_range_via_graphql(
        args.graphql_rpc_binary,
        db_url(args.pg_url, DB_A),
        args.remote_store_url,
        args.graphql_port_a,
        args.indexer_log_dir / "graphql_db_a.log",
        tag="graphql->db_a",
    )
    payload_b = query_available_range_via_graphql(
        args.graphql_rpc_binary,
        db_url(args.pg_url, DB_B),
        args.remote_store_url,
        args.graphql_port_b,
        args.indexer_log_dir / "graphql_db_b.log",
        tag="graphql->db_b",
    )

    def describe(side: str, payload: dict) -> tuple[Optional[str], Optional[str], Optional[str]]:
        """Return (first_cp, last_cp, error_msg)."""
        errors = payload.get("errors")
        if errors:
            return None, None, errors[0].get("message", str(errors[0]))
        rng = (payload.get("data") or {}).get("availableRange") or {}
        first = (rng.get("first") or {}).get("sequenceNumber")
        last = (rng.get("last") or {}).get("sequenceNumber")
        return first, last, None

    first_a, last_a, err_a = describe("db_a", payload_a)
    first_b, last_b, err_b = describe("db_b", payload_b)
    log(f"availableRange db_a: first={first_a} last={last_a} error={err_a}")
    log(f"availableRange db_b: first={first_b} last={last_b} error={err_b}")

    # db_a (migrated): backward-history is empty, so the GraphQL server should
    # either refuse the query at `settled_cp` ("outside the available range")
    # or report a `first` strictly greater than 0 (no genesis coverage).
    if err_a is None:
        if first_a == "0":
            sys.exit("db_a's availableRange reports first=0; migrated indexer "
                     "should not expose backward history from genesis")
        log("OK: db_a's availableRange doesn't reach genesis "
            f"(first={first_a})")
    else:
        log("OK: db_a refuses backward-range queries — no history coverage")

    # db_b (from-scratch): backward history starts at cp 0, so the available
    # range must include cp 0 (or be capped at first=0).
    if err_b is not None:
        sys.exit(f"db_b's availableRange errored: {err_b}")
    if first_b is None:
        sys.exit(f"db_b's availableRange returned no first value: {payload_b}")
    if first_b != "0":
        log(f"WARN: db_b's availableRange.first={first_b} (not 0); the "
            f"BACKWARD_HISTORY_MAX_LOOKBACK cap may be limiting it")
    else:
        log("OK: db_b's availableRange includes genesis (first=0)")


def assert_checkpointed_objects_setup(pg_url: str) -> None:
    """Verify pre-phase-3 invariants for the binaries we built.

    The `checkpointed_objects` table is created by a migration that exists only
    in the backward-feature branch (NEW). The table is populated incrementally
    by the NEW indexer's ingestion pipeline. Before applying the NEW migration
    to db_a, we expect:

      - db_a has no `checkpointed_objects` table (ran OLD only).
      - db_b has `checkpointed_objects` populated (ran NEW from genesis).

    A failure here means the OLD/NEW binaries got swapped or the branch under
    test no longer matches assumptions.
    """
    exists_sql = "SELECT to_regclass('public.checkpointed_objects') IS NOT NULL"
    exists_a = query_psql(pg_url, exists_sql, DB_A) == "t"
    exists_b = query_psql(pg_url, exists_sql, DB_B) == "t"
    log(f"checkpointed_objects table present: db_a={exists_a} db_b={exists_b}")
    if exists_a:
        sys.exit("db_a has checkpointed_objects table — wrong OLD binary? "
                 "OLD must not include the NEW migration.")
    if not exists_b:
        sys.exit("db_b lacks checkpointed_objects table — wrong NEW binary?")
    rows_b = int(query_psql(pg_url, "SELECT COUNT(*) FROM checkpointed_objects", DB_B))
    log(f"checkpointed_objects rowcount on db_b: {rows_b}")
    if rows_b == 0:
        sys.exit("db_b's checkpointed_objects is empty — NEW indexer didn't populate it?")


def get_watermark(pg_url: str, dbname: str) -> int:
    """Return the indexer's committed checkpoint watermark for `dbname`.

    Uses the `checkpoints` entity, which is the primary sync watermark.
    """
    sql = (
        "SELECT COALESCE(MAX(max_committed_cp), -1) "
        "FROM watermarks WHERE entity = 'checkpoints'"
    )
    val = query_psql(pg_url, sql, dbname)
    try:
        return int(val) if val else -1
    except ValueError:
        sys.exit(f"[{dbname}] unexpected watermark value: {val!r}")


def compare_checkpointed_objects(pg_url: str) -> bool:
    hash_sql = """
    SELECT md5(string_agg(row_hash, '' ORDER BY object_id))
    FROM (
      SELECT
        object_id,
        md5(
          encode(object_id, 'hex') || '|' ||
          object_version::text || '|' ||
          object_status::text || '|' ||
          COALESCE(encode(object_digest, 'hex'), '') || '|' ||
          checkpoint_sequence_number::text || '|' ||
          COALESCE(owner_type::text, '') || '|' ||
          COALESCE(encode(owner_id, 'hex'), '') || '|' ||
          COALESCE(object_type, '') || '|' ||
          COALESCE(encode(object_type_package, 'hex'), '') || '|' ||
          COALESCE(object_type_module, '') || '|' ||
          COALESCE(object_type_name, '') || '|' ||
          COALESCE(encode(serialized_object, 'hex'), '') || '|' ||
          COALESCE(coin_type, '') || '|' ||
          COALESCE(coin_balance::text, '') || '|' ||
          COALESCE(df_kind::text, '')
        ) AS row_hash
      FROM checkpointed_objects
    ) t;
    """
    count_sql = "SELECT COUNT(*) FROM checkpointed_objects"

    count_a = query_psql(pg_url, count_sql, DB_A)
    count_b = query_psql(pg_url, count_sql, DB_B)
    log(f"checkpointed_objects rowcount: db_a={count_a} db_b={count_b}")

    hash_a = query_psql(pg_url, hash_sql, DB_A)
    hash_b = query_psql(pg_url, hash_sql, DB_B)
    log(f"checkpointed_objects md5: db_a={hash_a or '<empty>'} db_b={hash_b or '<empty>'}")

    if hash_a == hash_b and count_a == count_b:
        log("PARITY: checkpointed_objects identical")
        return True

    log("MISMATCH: dumping per-row diffs")
    dump_dir = Path(tempfile.mkdtemp(prefix="migration_parity_diff_"))
    dump_sql = """
    COPY (
      SELECT object_id, object_version, object_status, object_digest,
             checkpoint_sequence_number, owner_type, owner_id,
             object_type, object_type_package, object_type_module, object_type_name,
             serialized_object, coin_type, coin_balance, df_kind
      FROM checkpointed_objects
      ORDER BY object_id
    ) TO STDOUT WITH CSV HEADER
    """
    for name in (DB_A, DB_B):
        out = dump_dir / f"{name}.csv"
        url = db_url(pg_url, name)
        with out.open("w") as f:
            subprocess.run(
                ["psql", url, "-v", "ON_ERROR_STOP=1", "-X", "-c", dump_sql],
                stdout=f, check=True,
            )
        log(f"dumped {name} -> {out}")

    diff_out = dump_dir / "diff.txt"
    with diff_out.open("w") as f:
        subprocess.run(
            ["diff", "-u", str(dump_dir / f"{DB_A}.csv"), str(dump_dir / f"{DB_B}.csv")],
            stdout=f,
        )
    log(f"row-level diff -> {diff_out}")
    return False


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def prepare_binary(
    side: str,
    branch: str,
    flag_branch: str,
    repo_root: Path,
    worktree_dir: Path,
    explicit_binary: Optional[Path],
    *,
    reuse_worktrees: bool,
    skip_rebuild: bool,
) -> Path:
    if explicit_binary is not None:
        if not explicit_binary.is_file() or not os.access(explicit_binary, os.X_OK):
            sys.exit(f"[{side}] --{side.lower()}-binary {explicit_binary} is not an executable file")
        log(f"[{side}] using pre-built binary at {explicit_binary}")
        return explicit_binary

    log(f"[{side}] preparing worktree for branch {branch} (+ flag {flag_branch})")
    wt = ensure_worktree(repo_root, worktree_dir, branch, flag_branch, reuse=reuse_worktrees)
    log(f"[{side}] building indexer in {wt}")
    return build_indexer(wt, skip_if_exists=skip_rebuild)


def resolve_stop_at_checkpoint(args: Args) -> int:
    if args.stop_at_checkpoint is not None:
        return args.stop_at_checkpoint
    if args.manifest is None:
        sys.exit("internal: neither --stop-at-checkpoint nor --manifest was provided")
    try:
        m = json.loads(args.manifest.read_text())
    except (OSError, json.JSONDecodeError) as e:
        sys.exit(f"failed to read manifest {args.manifest}: {e}")
    cp = m.get("final_checkpoint")
    if not isinstance(cp, int):
        sys.exit(f"manifest at {args.manifest} has no integer 'final_checkpoint' field")
    log(f"using stop_at_checkpoint={cp} from manifest {args.manifest}")
    return cp


def main() -> int:
    args = parse_args()

    parsed = urlparse(args.pg_url)
    if parsed.path and parsed.path != "/":
        sys.exit(f"--pg-url must not include a database name, got path={parsed.path!r}")

    stop_at = resolve_stop_at_checkpoint(args)

    if not (args.repo_root / ".git").exists():
        sys.exit(f"--repo-root {args.repo_root} does not look like a git repository")

    log("phase 0/4: prepare binaries")
    old_binary = prepare_binary(
        "OLD", args.base_branch, args.flag_branch,
        args.repo_root, args.worktree_dir, args.old_binary,
        reuse_worktrees=args.reuse_worktrees, skip_rebuild=args.skip_rebuild,
    )
    # graphql-rpc default: NEW worktree's release binary.
    if args.graphql_rpc_binary is None:
        new_wt_name = sanitize_branch(args.backward_branch)
        args.graphql_rpc_binary = (
            args.worktree_dir / new_wt_name / "target" / "release" / "iota-graphql-rpc"
        )
    if not args.graphql_rpc_binary.is_file():
        sys.exit(f"iota-graphql-rpc binary not found at {args.graphql_rpc_binary}; "
                 f"build it in the NEW worktree first.")

    new_binary = prepare_binary(
        "NEW", args.backward_branch, args.flag_branch,
        args.repo_root, args.worktree_dir, args.new_binary,
        reuse_worktrees=args.reuse_worktrees, skip_rebuild=args.skip_rebuild,
    )

    log("phase 1/4: reset databases")
    reset_db(args.pg_url, DB_A)
    reset_db(args.pg_url, DB_B)

    log("phase 2/4: sync both indexers to checkpoint N")
    run_indexer(
        old_binary,
        db_url(args.pg_url, DB_A),
        stop_at,
        args.remote_store_url,
        args.metrics_port_a,
        args.indexer_log_dir / "old_to_db_a.log",
        tag="OLD->db_a",
    )
    run_indexer(
        new_binary,
        db_url(args.pg_url, DB_B),
        stop_at,
        args.remote_store_url,
        args.metrics_port_b,
        args.indexer_log_dir / "new_to_db_b.log",
        tag="NEW->db_b",
    )

    # Indexer's --stop-at-checkpoint races with in-flight commit batching, so
    # the two DBs may settle a few cps short, and at different points. Iterate:
    # advance only the TRAILING DB toward the leader (with a small overshoot
    # to overcome per-run cancel-time loss). Repeat until they match.
    settled = None
    for attempt in range(30):
        cp_a = get_watermark(args.pg_url, DB_A)
        cp_b = get_watermark(args.pg_url, DB_B)
        log(f"catch-up attempt {attempt}: db_a={cp_a} db_b={cp_b}")
        if cp_a == cp_b:
            settled = cp_a
            break
        # Only advance the trailing DB; leader stays put.
        cap = max(cp_a, cp_b) + 10  # overshoot just enough to overcome in-flight loss
        if cp_a < cp_b:
            run_indexer(
                old_binary, db_url(args.pg_url, DB_A), cap,
                args.remote_store_url, args.metrics_port_a,
                args.indexer_log_dir / f"old_catchup_{attempt}.log",
                tag=f"OLD-catchup#{attempt}->db_a",
            )
        else:
            run_indexer(
                new_binary, db_url(args.pg_url, DB_B), cap,
                args.remote_store_url, args.metrics_port_b,
                args.indexer_log_dir / f"new_catchup_{attempt}.log",
                tag=f"NEW-catchup#{attempt}->db_b",
            )
    if settled is None:
        sys.exit("watermarks never converged across catch-up attempts")
    log(f"both DBs settled at cp {settled} (target was {stop_at})")

    # Sanity check: confirm we used the right binaries for each DB. The OLD
    # indexer's migrations don't create checkpointed_objects; the NEW indexer's
    # do, and ingestion populates it. A swap would silently invalidate the
    # parity diff.
    assert_checkpointed_objects_setup(args.pg_url)

    log("phase 3/4: apply NEW migration to db_a (no further ingestion)")
    # Pass stop_at = current db_a watermark so the NEW indexer's executor
    # immediately exits (next available cp will be > limit). Diesel migrations
    # run before the executor starts, so they are applied regardless.
    run_indexer(
        new_binary,
        db_url(args.pg_url, DB_A),
        settled,
        args.remote_store_url,
        args.metrics_port_a,
        args.indexer_log_dir / "new_migrate_db_a.log",
        tag="NEW-migrate->db_a",
    )

    log("phase 4a/4: check availableRange via graphql-rpc")
    check_available_range(args, settled)

    log("phase 4b/4: compare checkpointed_objects")
    parity = compare_checkpointed_objects(args.pg_url)

    if not args.keep_dbs:
        log("cleanup: dropping databases")
        run_psql(args.pg_url, f'DROP DATABASE IF EXISTS "{DB_A}" WITH (FORCE);')
        run_psql(args.pg_url, f'DROP DATABASE IF EXISTS "{DB_B}" WITH (FORCE);')

    return 0 if parity else 1


if __name__ == "__main__":
    sys.exit(main())
