#!/usr/bin/env python3
"""Query parity test for the backward-history consistency model (test plan #2).

Sets up two indexers + two GraphQL readers against their respective DBs:

  - db_old (forward path): synced by the OLD indexer; queried by the forward
    graphql-rpc (built from `--base-branch` + flag).
  - db_new (backward path): synced by the NEW indexer; queried by the backward
    graphql-rpc (built from `--backward-branch` + flag).

(The simpler "one indexer, two readers" setup the test plan also mentions
isn't viable: the forward graphql-rpc's diesel compatibility check rejects a
DB that's had the NEW indexer's migrations applied, since they're not present
in the OLD code's migration list.)

Issues a parametric query suite using object IDs from the workload manifest,
paginates each query, and asserts per-page JSON equality between the readers.

Per-query comparison is exact JSON-equal on the response body — pagination
follows cursors independently per reader but cursors are expected to encode the
same logical position in both paths.

Prerequisites:
  - Running postgres reachable at --pg-url (URL without a db name).
  - Running localnet reachable at --remote-store-url.
  - Local branches `base-branch`, `backward-branch`, `flag-branch` are present.
  - Workload manifest (produced by qa-workload) at --manifest.
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
from typing import Any, Iterator, Optional
from urllib.parse import urlparse


DB_OLD = "query_parity_old"
DB_NEW = "query_parity_new"

DEFAULT_BASE_BRANCH = "develop"
DEFAULT_BACKWARD_BRANCH = "infra/feat/backward-history-consistent-views"
DEFAULT_FLAG_BRANCH = "sc-platform/consistent-views-qa"


@dataclasses.dataclass
class Args:
    manifest: Path
    pg_url: str
    remote_store_url: str
    base_branch: str
    backward_branch: str
    flag_branch: str
    repo_root: Path
    worktree_dir: Path
    old_indexer_binary: Optional[Path]
    new_indexer_binary: Optional[Path]
    forward_graphql_binary: Optional[Path]
    backward_graphql_binary: Optional[Path]
    reuse_db: bool
    reuse_worktrees: bool
    skip_rebuild: bool
    page_size: int
    forward_port: int
    backward_port: int
    keep_db: bool
    metrics_port_old: int
    metrics_port_new: int
    log_dir: Path


def parse_args() -> Args:
    here = Path(__file__).resolve()
    default_repo = here.parents[2]
    default_worktree_dir = default_repo.parent / "migration_parity_worktrees"

    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--manifest", type=Path, required=True)
    p.add_argument("--pg-url", type=str, required=True)
    p.add_argument("--remote-store-url", type=str, required=True)
    p.add_argument("--base-branch", default=DEFAULT_BASE_BRANCH)
    p.add_argument("--backward-branch", default=DEFAULT_BACKWARD_BRANCH)
    p.add_argument("--flag-branch", default=DEFAULT_FLAG_BRANCH)
    p.add_argument("--repo-root", type=Path, default=default_repo)
    p.add_argument("--worktree-dir", type=Path, default=default_worktree_dir)
    p.add_argument("--old-indexer-binary", type=Path, default=None,
                   help="OLD iota-indexer (default: base worktree's target/release).")
    p.add_argument("--new-indexer-binary", type=Path, default=None,
                   help="NEW iota-indexer (default: backward worktree's target/release).")
    p.add_argument("--forward-graphql-binary", type=Path, default=None,
                   help="forward iota-graphql-rpc (default: base worktree's target/release).")
    p.add_argument("--backward-graphql-binary", type=Path, default=None,
                   help="backward iota-graphql-rpc (default: backward worktree's target/release).")
    p.add_argument("--reuse-db", action="store_true",
                   help="Reuse existing query_parity_old/new DBs (skip reset/sync).")
    p.add_argument("--reuse-worktrees", action="store_true")
    p.add_argument("--skip-rebuild", action="store_true")
    p.add_argument("--page-size", type=int, default=5)
    p.add_argument("--forward-port", type=int, default=18011)
    p.add_argument("--backward-port", type=int, default=18012)
    p.add_argument("--keep-db", action="store_true")
    p.add_argument("--metrics-port-old", type=int, default=19191)
    p.add_argument("--metrics-port-new", type=int, default=19192)
    p.add_argument("--log-dir", type=Path,
                   default=Path(tempfile.gettempdir()) / "query_parity_logs")
    a = p.parse_args()
    return Args(**vars(a))


def log(msg: str) -> None:
    print(f"[query-parity] {msg}", flush=True)


# ---------------------------------------------------------------------------
# Postgres / indexer plumbing (lightweight; full versions in migration_parity)
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


def get_watermark(pg_url: str, dbname: str) -> int:
    val = query_psql(
        pg_url,
        "SELECT COALESCE(MAX(max_committed_cp), -1) FROM watermarks WHERE entity = 'checkpoints'",
        dbname,
    )
    try:
        return int(val) if val else -1
    except ValueError:
        sys.exit(f"unexpected watermark value: {val!r}")


def run_indexer(binary: Path, db_full: str, stop_at: int,
                remote_store_url: str, metrics_port: int, log_path: Path, tag: str) -> None:
    cmd = [
        str(binary),
        f"--database-url={db_full}",
        f"--metrics-address=0.0.0.0:{metrics_port}",
        "indexer",
        f"--remote-store-url={remote_store_url}",
        f"--stop-at-checkpoint={stop_at}",
    ]
    log(f"[{tag}] starting indexer to cp {stop_at}")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with log_path.open("w") as lf:
        rc = subprocess.run(cmd, stdout=lf, stderr=subprocess.STDOUT).returncode
    if rc != 0:
        sys.exit(f"[{tag}] indexer exited non-zero ({rc}); see {log_path}")


# ---------------------------------------------------------------------------
# Worktree + build management
# ---------------------------------------------------------------------------


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
    # Cherry-pick every commit on flag_branch that isn't already in HEAD (the
    # branch carries multiple commits, of which the indexer code changes need
    # to be applied for the build).
    commits = subprocess.run(
        ["git", "rev-list", "--reverse", f"HEAD..{flag_branch}"],
        cwd=wt_path, check=True, capture_output=True, text=True,
    ).stdout.strip().split()
    if commits:
        try:
            subprocess.run(["git", "cherry-pick"] + commits, cwd=wt_path, check=True)
        except subprocess.CalledProcessError as e:
            subprocess.run(["git", "cherry-pick", "--abort"], cwd=wt_path, check=False)
            sys.exit(f"cherry-pick of {len(commits)} commits into {branch} failed: {e}")
    return wt_path


def build_in_worktree(worktree: Path, packages: list[str], skip_if_exists: bool,
                      expect_binaries: list[str]) -> None:
    if skip_if_exists and all((worktree / "target" / "release" / b).exists()
                              for b in expect_binaries):
        log(f"reusing existing binaries in {worktree}")
        return
    cmd = ["cargo", "build", "--release"]
    for p in packages:
        cmd += ["-p", p]
    log(f"building {packages} in {worktree}")
    subprocess.run(cmd, cwd=worktree, check=True)


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
    proc._lf = lf  # type: ignore[attr-defined]  # keep handle to close later
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
# Query suite + paginated comparison
# ---------------------------------------------------------------------------


def page_through(url: str, query: str, base_vars: dict, path: Optional[list[str]],
                 page_size: int) -> list[dict]:
    """Run query against `url`, returning a list of response JSON pages.

    If `path` is None the query is non-paginated: a single response is returned.
    Otherwise iterate Connection-style pagination — `path` is the list of keys
    to descend into the response to find the `{ pageInfo, ... }` object.
    """
    if path is None:
        return [graphql_post(url, query, base_vars)]
    pages = []
    cursor = None
    while True:
        vars_ = dict(base_vars)
        vars_["first"] = page_size
        vars_["after"] = cursor
        r = graphql_post(url, query, vars_)
        if "errors" in r:
            pages.append(r)
            return pages
        pages.append(r)
        node = r["data"]
        for k in path:
            if node is None:
                return pages
            node = node.get(k)
        if node is None:
            return pages
        page_info = node.get("pageInfo") or {}
        if not page_info.get("hasNextPage"):
            return pages
        cursor = page_info.get("endCursor")
    return pages


def diff_responses(a: dict, b: dict) -> Optional[str]:
    """Return None if equal; else a short string describing the first diff."""
    if a == b:
        return None
    # Use json.dumps with sort_keys for stable rendering.
    sa = json.dumps(a, sort_keys=True, indent=2)
    sb = json.dumps(b, sort_keys=True, indent=2)
    if sa == sb:
        return None  # equivalent up to key order
    # Find first differing line.
    la = sa.splitlines()
    lb = sb.splitlines()
    for i, (x, y) in enumerate(zip(la, lb)):
        if x != y:
            ctx = max(0, i - 2)
            return (
                f"first diff at line {i + 1}\n"
                f"--- forward ---\n" + "\n".join(la[ctx:i + 5]) + "\n"
                f"--- backward ---\n" + "\n".join(lb[ctx:i + 5])
            )
    return f"length mismatch: forward={len(la)} lines, backward={len(lb)} lines"


def compare_paged(fwd_url: str, bwd_url: str, name: str, query: str,
                  vars_: dict, path: Optional[list[str]], page_size: int) -> tuple[int, int]:
    """Page through `query` on both URLs in lockstep, return (matches, mismatches)."""
    fwd_pages = page_through(fwd_url, query, vars_, path, page_size)
    bwd_pages = page_through(bwd_url, query, vars_, path, page_size)
    matches = mismatches = 0
    n = max(len(fwd_pages), len(bwd_pages))
    for i in range(n):
        fwd = fwd_pages[i] if i < len(fwd_pages) else {"missing": "forward"}
        bwd = bwd_pages[i] if i < len(bwd_pages) else {"missing": "backward"}
        diff = diff_responses(fwd, bwd)
        if diff is None:
            matches += 1
        else:
            mismatches += 1
            log(f"  [{name}] page {i + 1}/{n} MISMATCH:\n{diff}")
    log(f"[{name}] pages: {matches} match, {mismatches} differ")
    return matches, mismatches


# ---------------------------------------------------------------------------
# Query templates (one connection per query for pagination)
# ---------------------------------------------------------------------------


def get_object_version(url: str, object_id: str) -> Optional[int]:
    r = graphql_post(url, "query Q($id: IotaAddress!) { object(address: $id) { version } }",
                     {"id": object_id})
    obj = (r.get("data") or {}).get("object")
    if obj is None:
        return None
    return int(obj["version"])


def query_suite(manifest: dict, fwd_url: str) -> list[tuple[str, str, dict, Optional[list[str]]]]:
    """Return [(name, query, vars, response_path_to_connection_or_None)].

    `fwd_url` is used to discover current object versions for version-pinned
    queries — both readers are queried at the same numeric version so any
    disagreement surfaces as a parity mismatch on the actual query.
    """
    scens = manifest["scenarios"]
    pkg = manifest["package_id"]
    sender = manifest["sender"]

    suite: list[tuple[str, str, dict, Optional[list[str]]]] = []

    # Pick a few representative object ids for per-object queries.
    df_parent = scens["parents_with_dfs"][0]
    dof_parent = scens["parents_with_dofs"][0]

    # Q1: dynamic fields on a DF-rich parent (current view).
    suite.append((
        "object.dynamicFields(df_parent)",
        """
        query Q($id: IotaAddress!, $first: Int, $after: String) {
          object(address: $id) {
            address
            version
            dynamicFields(first: $first, after: $after) {
              pageInfo { hasNextPage endCursor }
              nodes { name { type { repr } bcs } value { __typename ... on MoveValue { type { repr } bcs } } }
            }
          }
        }
        """,
        {"id": df_parent},
        ["object", "dynamicFields"],
    ))

    # Q2: dynamic fields on a DOF-rich parent.
    suite.append((
        "object.dynamicFields(dof_parent)",
        """
        query Q($id: IotaAddress!, $first: Int, $after: String) {
          object(address: $id) {
            address
            version
            dynamicFields(first: $first, after: $after) {
              pageInfo { hasNextPage endCursor }
              nodes { name { type { repr } bcs } value { __typename } }
            }
          }
        }
        """,
        {"id": dof_parent},
        ["object", "dynamicFields"],
    ))

    # Q3: paginated objects filtered by Parent type.
    parent_type = f"{pkg}::workload::Parent"
    suite.append((
        "objects(filter Parent type)",
        f"""
        query Q($first: Int, $after: String) {{
          objects(
            filter: {{ type: "{parent_type}" }}
            first: $first
            after: $after
          ) {{
            pageInfo {{ hasNextPage endCursor }}
            nodes {{ address version }}
          }}
        }}
        """,
        {},
        ["objects"],
    ))

    # Q4: coins owned by sender.
    suite.append((
        "address.coinObjects",
        """
        query Q($addr: IotaAddress!, $first: Int, $after: String) {
          address(address: $addr) {
            coinObjects: coins(first: $first, after: $after) {
              pageInfo { hasNextPage endCursor }
              nodes { coinObjectId: address version balance }
            }
          }
        }
        """,
        {"addr": sender},
        ["address", "coinObjects"],
    ))

    # Q5..N: version-pinned DF queries on parents_with_dfs (the migration's
    # motivating case). Probe current version + a few historical versions.
    for parent in scens["parents_with_dfs"][:3]:
        cur_ver = get_object_version(fwd_url, parent)
        if cur_ver is None:
            log(f"WARN: couldn't fetch current version of {parent}; skipping version-pinned probes")
            continue
        # Sample versions: current, current-1, mid-history, very early.
        targets = sorted({cur_ver, max(1, cur_ver - 1), max(1, cur_ver - 5), 3})
        for v in targets:
            suite.append((
                f"object(v={v}).dynamicFields({parent[:10]})",
                """
                query Q($id: IotaAddress!, $v: UInt53!, $first: Int, $after: String) {
                  object(address: $id, version: $v) {
                    address
                    version
                    dynamicFields(first: $first, after: $after) {
                      pageInfo { hasNextPage endCursor }
                      nodes { name { type { repr } bcs } value { __typename } }
                    }
                  }
                }
                """,
                {"id": parent, "v": v},
                ["object", "dynamicFields"],
            ))

    # Q: tombstone lookup — wrapped_then_deleted children should be reported
    # consistently (either null or with a tombstone status) by both readers.
    for child_id in scens["wrapped_then_deleted"][:3]:
        suite.append((
            f"object(wrapped_deleted={child_id[:10]})",
            """
            query Q($id: IotaAddress!) {
              object(address: $id) {
                address
                version
                digest
                status
                owner { __typename }
              }
            }
            """,
            {"id": child_id},
            None,  # non-paginated
        ))

    # Q: transferred-objects ownership filter — list objects currently owned by
    # the transfer recipient and confirm the transferred parents show up. All
    # workload transfers used the same recipient address.
    recipients = sorted({rec["to"] for rec in scens["transfer_chain"]})
    for recipient in recipients:
        suite.append((
            f"objects(filter owner={recipient[:10]})",
            """
            query Q($owner: IotaAddress!, $first: Int, $after: String) {
              objects(filter: { owner: $owner }, first: $first, after: $after) {
                pageInfo { hasNextPage endCursor }
                nodes { address version }
              }
            }
            """,
            {"owner": recipient},
            ["objects"],
        ))

    return suite


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def resolve_default_binaries(args: Args) -> None:
    """Fill in indexer/graphql binary paths from the worktrees if not provided."""
    base_wt = args.worktree_dir / sanitize_branch(args.base_branch)
    new_wt = args.worktree_dir / sanitize_branch(args.backward_branch)
    if args.old_indexer_binary is None:
        args.old_indexer_binary = base_wt / "target" / "release" / "iota-indexer"
    if args.new_indexer_binary is None:
        args.new_indexer_binary = new_wt / "target" / "release" / "iota-indexer"
    if args.forward_graphql_binary is None:
        args.forward_graphql_binary = base_wt / "target" / "release" / "iota-graphql-rpc"
    if args.backward_graphql_binary is None:
        args.backward_graphql_binary = new_wt / "target" / "release" / "iota-graphql-rpc"


def main() -> int:
    args = parse_args()

    parsed = urlparse(args.pg_url)
    if parsed.path and parsed.path != "/":
        sys.exit(f"--pg-url must not include a database name, got path={parsed.path!r}")

    manifest = json.loads(args.manifest.read_text())
    target_cp = int(manifest["final_checkpoint"])
    log(f"using manifest cp {target_cp}, package={manifest['package_id']}")

    # Phase 0: prepare worktrees + binaries (NEW indexer + forward graphql +
    # backward graphql).
    log("phase 0: prepare binaries")
    if not args.reuse_db:
        ensure_worktree(args.repo_root, args.worktree_dir,
                        args.backward_branch, args.flag_branch, reuse=args.reuse_worktrees)
        build_in_worktree(args.worktree_dir / sanitize_branch(args.backward_branch),
                          ["iota-indexer", "iota-node", "iota-graphql-rpc"],
                          skip_if_exists=args.skip_rebuild,
                          expect_binaries=["iota-indexer", "iota-graphql-rpc"])
    ensure_worktree(args.repo_root, args.worktree_dir,
                    args.base_branch, args.flag_branch, reuse=args.reuse_worktrees)
    build_in_worktree(args.worktree_dir / sanitize_branch(args.base_branch),
                      ["iota-indexer", "iota-node", "iota-graphql-rpc"],
                      skip_if_exists=args.skip_rebuild,
                      expect_binaries=["iota-graphql-rpc"])
    resolve_default_binaries(args)
    for b in (args.old_indexer_binary, args.new_indexer_binary,
              args.forward_graphql_binary, args.backward_graphql_binary):
        if not b.is_file():
            sys.exit(f"binary not found: {b}")

    # Phase 1: provision both DBs and catch them up to the same watermark.
    if not args.reuse_db:
        log(f"phase 1: sync OLD->{DB_OLD} and NEW->{DB_NEW} to >= cp {target_cp}")
        reset_db(args.pg_url, DB_OLD)
        reset_db(args.pg_url, DB_NEW)
        run_indexer(args.old_indexer_binary, db_url(args.pg_url, DB_OLD), target_cp,
                    args.remote_store_url, args.metrics_port_old,
                    args.log_dir / "old_sync.log", tag=f"OLD->{DB_OLD}")
        run_indexer(args.new_indexer_binary, db_url(args.pg_url, DB_NEW), target_cp,
                    args.remote_store_url, args.metrics_port_new,
                    args.log_dir / "new_sync.log", tag=f"NEW->{DB_NEW}")
    else:
        log("phase 1: reusing existing dbs (catching up if needed)")

    # Catch-up loop: advance trailing DB until watermarks match.
    for attempt in range(60):
        cp_old = get_watermark(args.pg_url, DB_OLD)
        cp_new = get_watermark(args.pg_url, DB_NEW)
        log(f"catch-up #{attempt}: old={cp_old} new={cp_new}")
        if cp_old == cp_new:
            break
        cap = max(cp_old, cp_new) + 10
        if cp_old < cp_new:
            run_indexer(args.old_indexer_binary, db_url(args.pg_url, DB_OLD), cap,
                        args.remote_store_url, args.metrics_port_old,
                        args.log_dir / f"old_catchup_{attempt}.log",
                        tag=f"OLD-catchup#{attempt}")
        else:
            run_indexer(args.new_indexer_binary, db_url(args.pg_url, DB_NEW), cap,
                        args.remote_store_url, args.metrics_port_new,
                        args.log_dir / f"new_catchup_{attempt}.log",
                        tag=f"NEW-catchup#{attempt}")
    else:
        sys.exit("watermarks didn't converge after 60 attempts")
    settled = get_watermark(args.pg_url, DB_OLD)
    log(f"both DBs settled at cp {settled}")

    # Phase 2: start both graphql-rpc servers against their respective DBs.
    log("phase 2: start graphql readers")
    fwd_proc = start_graphql_rpc(args.forward_graphql_binary, db_url(args.pg_url, DB_OLD),
                                  args.remote_store_url, args.forward_port,
                                  args.log_dir / "graphql_forward.log", tag="forward")
    bwd_proc = start_graphql_rpc(args.backward_graphql_binary, db_url(args.pg_url, DB_NEW),
                                  args.remote_store_url, args.backward_port,
                                  args.log_dir / "graphql_backward.log", tag="backward")

    fwd_url = f"http://127.0.0.1:{args.forward_port}/graphql"
    bwd_url = f"http://127.0.0.1:{args.backward_port}/graphql"

    total_match = total_mismatch = 0
    try:
        # Phase 3: run query suite.
        log("phase 3: run query suite (per-page diff)")
        suite = query_suite(manifest, fwd_url)
        for name, query, vars_, path in suite:
            m, mm = compare_paged(fwd_url, bwd_url, name, query, vars_, path, args.page_size)
            total_match += m
            total_mismatch += mm
    finally:
        stop_graphql_rpc(fwd_proc)
        stop_graphql_rpc(bwd_proc)

    log(f"summary: {total_match} pages match, {total_mismatch} differ")

    if not args.keep_db and not args.reuse_db:
        log(f"cleanup: dropping {DB_OLD} and {DB_NEW}")
        run_psql(args.pg_url, f'DROP DATABASE IF EXISTS "{DB_OLD}" WITH (FORCE);')
        run_psql(args.pg_url, f'DROP DATABASE IF EXISTS "{DB_NEW}" WITH (FORCE);')

    return 0 if total_mismatch == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
