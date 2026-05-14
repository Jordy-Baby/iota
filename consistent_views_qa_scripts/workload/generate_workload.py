#!/usr/bin/env python3
"""Generate a coverage workload on a running localnet.

Publishes the `workload::workload` Move package once, then drives a fixed set
of scenarios that produce diverse object lifecycles and dynamic-field histories
useful for consistent-views QA tests:

  1. parents_with_dfs            10 parents x 10 DFs each
  2. parents_with_dofs           10 parents x 10 DOFs each
  3. df_lifecycle                3 parents going add->remove->add across cps
  4. transfer_chain              10 objects transferred between two addresses
  5. wrapped_then_unwrapped      10 children wrap -> unwrap
  6. wrapped_then_deleted        10 children wrapped + deleted (tombstone)
  7. deleted                     10 objects plain-deleted
  8. borrow_mut_quirk            3 DOF children mutated via borrow_mut

At the end, prints a JSON manifest of object IDs grouped by scenario, plus the
latest checkpoint sequence number the localnet reached. Pipe-friendly: pass
`--output <path>` to also write the manifest to disk.

Prerequisites:
  - `iota` CLI on PATH, configured with an `iota client active-env` pointing at
    your localnet (or pass --rpc-url to add one ad-hoc).
  - Localnet faucet reachable.
  - Active address has, or can request, enough gas.

Usage:
  ./generate_workload.py \\
    --rpc-url http://localhost:9000 \\
    --faucet-url http://localhost:9123/gas \\
    --output /tmp/workload_manifest.json
"""

import argparse
import dataclasses
import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path
from typing import Any, Optional


GAS_BUDGET = 100_000_000
DEFAULT_ENV_ALIAS = "qa-workload-localnet"


@dataclasses.dataclass
class Args:
    rpc_url: str
    faucet_url: str
    package_path: Path
    output: Optional[Path]
    env_alias: str
    skip_publish: Optional[str]  # pre-known package id; skip publish


def parse_args() -> Args:
    here = Path(__file__).resolve().parent
    default_pkg = here / "move_package"
    p = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--rpc-url", default="http://localhost:9000")
    p.add_argument("--faucet-url", default="http://localhost:9123/gas")
    p.add_argument("--package-path", type=Path, default=default_pkg)
    p.add_argument("--output", type=Path, default=None,
                   help="Write JSON manifest to this path in addition to stdout.")
    p.add_argument("--env-alias", default=DEFAULT_ENV_ALIAS,
                   help="iota client env alias to use/create.")
    p.add_argument("--skip-publish", default=None,
                   help="Reuse an already-published package id; skip publishing.")
    a = p.parse_args()
    return Args(**vars(a))


def log(msg: str) -> None:
    print(f"[workload] {msg}", flush=True, file=sys.stderr)


def jrpc(rpc_url: str, method: str, params: Optional[list] = None) -> Any:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []}).encode()
    req = urllib.request.Request(rpc_url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        out = json.loads(r.read())
    if "error" in out:
        raise RuntimeError(f"{method} failed: {out['error']}")
    return out["result"]


def run_cli(args: list[str], *, capture: bool = True, check: bool = True,
            retries: int = 5, retry_delay: float = 2.0) -> str:
    """Run an `iota` CLI command; retry on transient connect failures."""
    cmd = ["iota", *args]
    last = None
    for attempt in range(retries):
        r = subprocess.run(cmd, check=False, capture_output=capture, text=True)
        last = r
        if r.returncode == 0:
            return r.stdout
        transient = "Connection refused" in (r.stdout + r.stderr) \
            or "tcp connect error" in (r.stdout + r.stderr) \
            or "request timed out" in (r.stdout + r.stderr).lower()
        if not transient or attempt == retries - 1:
            break
        log(f"transient CLI error (attempt {attempt + 1}/{retries}); retrying in {retry_delay}s")
        time.sleep(retry_delay)
    if check and last is not None and last.returncode != 0:
        sys.exit(f"command failed: {' '.join(cmd)}\nSTDOUT:\n{last.stdout}\nSTDERR:\n{last.stderr}")
    return last.stdout if last else ""


def cli_json(args: list[str]) -> Any:
    raw = run_cli([*args, "--json"])
    try:
        return json.loads(raw)
    except json.JSONDecodeError as e:
        sys.exit(f"failed to parse iota CLI JSON output: {e}\nRaw: {raw[:500]}")


# ---------------------------------------------------------------------------
# Environment setup
# ---------------------------------------------------------------------------


def ensure_env(env_alias: str, rpc_url: str) -> None:
    """Create or switch to the local QA env so we know which RPC we're talking to."""
    # `iota client envs --json` returns [[env, ...], active_alias_or_null].
    raw = cli_json(["client", "envs"])
    env_list = raw[0] if isinstance(raw, list) and raw and isinstance(raw[0], list) else raw
    matching = [e for e in env_list if isinstance(e, dict) and e.get("alias") == env_alias]
    if not matching:
        log(f"creating new env '{env_alias}' -> {rpc_url}")
        run_cli(["client", "new-env", "--alias", env_alias, "--rpc", rpc_url])
    elif matching[0].get("rpc") != rpc_url:
        sys.exit(f"env '{env_alias}' exists but points to {matching[0].get('rpc')}, not {rpc_url}; "
                 f"choose a different --env-alias or delete the existing env.")
    log(f"switching to env '{env_alias}'")
    run_cli(["client", "switch", "--env", env_alias])


def active_address() -> str:
    raw = run_cli(["client", "active-address"]).strip()
    if not raw.startswith("0x"):
        sys.exit(f"unexpected active-address output: {raw!r}")
    return raw


def request_faucet(faucet_url: str, address: str) -> None:
    body = json.dumps({"FixedAmountRequest": {"recipient": address}}).encode()
    req = urllib.request.Request(faucet_url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        _ = r.read()
    log(f"faucet -> {address}")


def ensure_funded(faucet_url: str, address: str, min_coins: int = 5) -> None:
    """Hit the faucet until at least `min_coins` gas coins are owned."""
    for _ in range(min_coins):
        gas = cli_json(["client", "gas"])
        if isinstance(gas, list) and len(gas) >= min_coins:
            return
        request_faucet(faucet_url, address)
        time.sleep(1.0)


# ---------------------------------------------------------------------------
# Effects parsing
# ---------------------------------------------------------------------------


def _objs_with_type(effects: dict, kind: str, type_substr: str) -> list[str]:
    """Return objectIds of `kind` items whose type contains `type_substr`.

    `objectChanges` is the most reliable source — it always carries the type.
    """
    out = []
    for ch in effects.get("objectChanges") or []:
        if ch.get("type") != kind:
            continue
        if type_substr in (ch.get("objectType") or ""):
            out.append(ch["objectId"])
    return out


def created_of_type(effects: dict, type_substr: str) -> list[str]:
    return _objs_with_type(effects, "created", type_substr)


def mutated_of_type(effects: dict, type_substr: str) -> list[str]:
    return _objs_with_type(effects, "mutated", type_substr)


def first_created_of_type(effects: dict, type_substr: str) -> str:
    ids = created_of_type(effects, type_substr)
    if not ids:
        sys.exit(f"no created object of type containing {type_substr!r} found in effects")
    return ids[0]


def status_ok(effects: dict) -> None:
    s = ((effects.get("effects") or {}).get("status") or {}).get("status")
    if s != "success":
        sys.exit(f"tx did not succeed: status={s!r}\n{json.dumps(effects, indent=2)[:1500]}")


_tx_counter = 0


def call_move(pkg: str, module: str, function: str, args: list[str],
              *, type_args: Optional[list[str]] = None) -> dict:
    global _tx_counter
    _tx_counter += 1
    if _tx_counter % 10 == 0:
        log(f"  tx #{_tx_counter} ({module}::{function})")
    cmd = [
        "client", "call",
        "--package", pkg,
        "--module", module,
        "--function", function,
        "--gas-budget", str(GAS_BUDGET),
    ]
    for a in args:
        cmd += ["--args", a]
    if type_args:
        for t in type_args:
            cmd += ["--type-args", t]
    out = cli_json(cmd)
    status_ok(out)
    return out


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------


def publish_package(pkg_path: Path) -> str:
    log(f"publishing package from {pkg_path}")
    out = cli_json([
        "client", "publish",
        "--gas-budget", str(GAS_BUDGET),
        str(pkg_path),
    ])
    status_ok(out)
    pkgs = [c["packageId"] for c in (out.get("objectChanges") or [])
            if c.get("type") == "published"]
    if not pkgs:
        sys.exit("no package was published according to effects")
    log(f"package published: {pkgs[0]}")
    return pkgs[0]


def scenario_parents_with_dfs(pkg: str, n_parents: int = 10, n_fields: int = 10) -> list[str]:
    log(f"scenario 1: {n_parents} parents x {n_fields} DFs")
    parents = []
    for i in range(n_parents):
        out = call_move(pkg, "workload", "mint_parent_to_sender", [str(i)])
        parents.append(first_created_of_type(out, "::workload::Parent"))
    for parent in parents:
        for k in range(n_fields):
            call_move(pkg, "workload", "add_df", [parent, str(k), str(k * 7)])
    return parents


def scenario_parents_with_dofs(pkg: str, n_parents: int = 10, n_fields: int = 10) -> list[str]:
    log(f"scenario 2: {n_parents} parents x {n_fields} DOFs")
    parents = []
    for i in range(n_parents):
        out = call_move(pkg, "workload", "mint_parent_to_sender", [str(100 + i)])
        parents.append(first_created_of_type(out, "::workload::Parent"))
    for parent in parents:
        for k in range(n_fields):
            call_move(pkg, "workload", "add_dof_to", [parent, str(k), str(k * 11)])
    return parents


def scenario_df_lifecycle(pkg: str, n_parents: int = 3) -> list[str]:
    log(f"scenario 3: DF add->remove->add lifecycle on {n_parents} parents")
    parents = []
    for i in range(n_parents):
        out = call_move(pkg, "workload", "mint_parent_to_sender", [str(200 + i)])
        parents.append(first_created_of_type(out, "::workload::Parent"))
    # round 1: add
    for parent in parents:
        call_move(pkg, "workload", "add_df", [parent, "42", "1000"])
    # round 2: remove (separate cp window)
    for parent in parents:
        call_move(pkg, "workload", "remove_df", [parent, "42"])
    # round 3: re-add same key
    for parent in parents:
        call_move(pkg, "workload", "add_df", [parent, "42", "2000"])
    return parents


def scenario_transfer_chain(pkg: str, recipient: str, n: int = 10) -> list[dict]:
    log(f"scenario 4: {n} transfer chains to {recipient[:10]}...")
    results = []
    for i in range(n):
        out = call_move(pkg, "workload", "mint_parent_to_sender", [str(300 + i)])
        obj_id = first_created_of_type(out, "::workload::Parent")
        # transfer (single-hop is sufficient to record an owner change).
        run_cli([
            "client", "transfer",
            "--to", recipient,
            "--object-id", obj_id,
            "--gas-budget", str(GAS_BUDGET),
            "--json",
        ])
        results.append({"object": obj_id, "to": recipient})
    return results


def scenario_wrap_unwrap(pkg: str, n: int = 10) -> list[str]:
    log(f"scenario 5: {n} wrap/unwrap roundtrips")
    children = []
    for i in range(n):
        out = call_move(pkg, "workload", "mint_child_to_sender", [str(400 + i)])
        child_id = first_created_of_type(out, "::workload::Child")
        # wrap and immediately unwrap (creates wrapper, then destroys it)
        wrap_out = call_move(pkg, "workload", "wrap_self_and_send", [child_id])
        wrapper_id = first_created_of_type(wrap_out, "::workload::Wrapper")
        call_move(pkg, "workload", "unwrap_and_send", [wrapper_id])
        children.append(child_id)
    return children


def scenario_wrap_delete(pkg: str, n: int = 10) -> list[str]:
    log(f"scenario 6: {n} wrap+delete (tombstone)")
    children = []
    for i in range(n):
        out = call_move(pkg, "workload", "mint_child_to_sender", [str(500 + i)])
        child_id = first_created_of_type(out, "::workload::Child")
        call_move(pkg, "workload", "wrap_and_delete", [child_id])
        children.append(child_id)
    return children


def scenario_plain_delete(pkg: str, n: int = 10) -> list[str]:
    log(f"scenario 7: {n} plain deletes")
    parents = []
    for i in range(n):
        out = call_move(pkg, "workload", "mint_parent_to_sender", [str(600 + i)])
        parent_id = first_created_of_type(out, "::workload::Parent")
        call_move(pkg, "workload", "delete_parent", [parent_id])
        parents.append(parent_id)
    return parents


def scenario_borrow_mut_quirk(pkg: str, n: int = 3) -> list[dict]:
    log(f"scenario 8: {n} borrow_mut quirk pairs")
    results = []
    for i in range(n):
        out = call_move(pkg, "workload", "mint_parent_to_sender", [str(700 + i)])
        parent = first_created_of_type(out, "::workload::Parent")
        # add a DOF first
        add_out = call_move(pkg, "workload", "add_dof_to", [parent, "0", "777"])
        dof_child = first_created_of_type(add_out, "::workload::Child")
        # then mutate the DOF child via borrow_mut (parent version stays put)
        call_move(pkg, "workload", "add_df_to_dof_child", [parent, "0", "1", "999"])
        results.append({"parent": parent, "dof_child": dof_child})
    return results


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    args = parse_args()

    ensure_env(args.env_alias, args.rpc_url)
    sender = active_address()
    log(f"sender: {sender}")
    ensure_funded(args.faucet_url, sender)

    if args.skip_publish:
        pkg = args.skip_publish
        log(f"using existing package id: {pkg}")
    else:
        pkg = publish_package(args.package_path)

    # Use a deterministic non-sender address for transfers. The address need
    # not be controllable; transfers are one-shot and we don't care about
    # spending the recipient's tokens.
    transfer_recipient = "0x" + "ab" * 32

    manifest: dict[str, Any] = {
        "package_id": pkg,
        "sender": sender,
        "scenarios": {},
    }

    manifest["scenarios"]["parents_with_dfs"] = scenario_parents_with_dfs(pkg)
    # re-fund partway through; faucet has rate limits but we drain coins
    ensure_funded(args.faucet_url, sender)
    manifest["scenarios"]["parents_with_dofs"] = scenario_parents_with_dofs(pkg)
    ensure_funded(args.faucet_url, sender)
    manifest["scenarios"]["df_lifecycle_parents"] = scenario_df_lifecycle(pkg)
    manifest["scenarios"]["transfer_chain"] = scenario_transfer_chain(pkg, transfer_recipient)
    manifest["scenarios"]["wrapped_then_unwrapped"] = scenario_wrap_unwrap(pkg)
    manifest["scenarios"]["wrapped_then_deleted"] = scenario_wrap_delete(pkg)
    manifest["scenarios"]["deleted"] = scenario_plain_delete(pkg)
    manifest["scenarios"]["borrow_mut_pairs"] = scenario_borrow_mut_quirk(pkg)

    # Final checkpoint number.
    cp = jrpc(args.rpc_url, "iota_getLatestCheckpointSequenceNumber")
    manifest["final_checkpoint"] = int(cp)
    log(f"final checkpoint: {cp}")

    out_text = json.dumps(manifest, indent=2)
    print(out_text)
    if args.output:
        args.output.write_text(out_text)
        log(f"manifest written to {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
