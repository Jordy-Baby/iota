#!/usr/bin/env python3
"""Query performance test for the backward-history consistency view.

Issues paginated `objects(...)` GraphQL queries across a sweep of cursor
`checkpoint_viewed_at` values, measuring per-query latency as a function of
lookback distance K.

The cursor encodes `(object_id, checkpoint_viewed_at)` as BCS+base64; the
graphql-rpc derefs `checkpoint_viewed_at` from the cursor and uses it as the
consistent upper bound (see `iota-graphql-rpc/src/types/object.rs:864`).
Crafting cursors with chosen `c` exercises arbitrary lookback distances
within the `[max(min_available_cp, latest - BACKWARD_HISTORY_MAX_LOOKBACK),
latest]` window without waiting for real time.

Eight query shapes are supported, each parameterised by a worst-case filter
value on staging:

  ids             objects(filter: {objectIds: [...]}, first: 2)
  type            objects(filter: {type: T}, first: 50)
  owner           objects(filter: {owner: O}, first: 50)
  empty           objects(first: 50)
  version-pin     object(address: A, version: V)  (sweeps version recency)
  object-keys     objects(filter: {objectKeys: [{objectId: A, version: V}]}, first: 1)
  dynamic-fields  object(address: A, version: V) { dynamicFields(first: 50) ... }
  all             run all of the above in sequence

Cursor object_id is always 32 zero bytes, so the `after` filter passes every
candidate through; only `checkpoint_viewed_at` differs per sample.

`version-pin` doesn't use a cursor — it pins via the top-level
`object(address, version)` query, sweeping `version` across the target
object's history. Routes via `Object::at_version` → the legacy
`HistoricalKey` Loader at `object.rs:1529`, which joins `objects_history` ×
`objects_version` (OLD code path, not the new backward-history view).

`object-keys` exercises the NEW backward-view historical path
(`backward_view/historical.rs::query`): it UNIONs `checkpointed_objects` +
`objects_backward_history` + tombstone rows from `objects_version`. Routes
via `objects(filter: {objectKeys: [...]})` → `backward_objects_query` →
`HistoricalFilter::try_from(filter)` → `historical::query`.

`dynamic-fields` exercises `backward_view/dynamic_fields.rs::query` — the
parent-version-pinned DF view that merges `checkpointed_objects` and
`objects_backward_history` rows where `owner_id = parent AND owner_type =
Object AND df_kind IS NOT NULL`. Routes via top-level `owner(address,
rootVersion=V)` (a synthetic Owner that always resolves — no parent lookup)
→ `Owner.dynamicFields` → `DynamicField::paginate(parent_version: Some(V))`
→ `dynamic_fields::query`. Using `owner(...)` instead of `object(addr, v)`
lets us target an address that owns DFs but isn't itself a top-level
addressable object (e.g. the wrapped inner Versioned of Random on staging).

Run against a NEW iota-graphql-rpc that is backed by a NEW-indexed DB. For
stable measurements stop the concurrent indexer first — with ingestion
active the available window moves between probe and query and deep-K
samples may fall outside the window.
"""

import argparse
import base64
import json
import statistics
import struct
import sys
import time
import urllib.request
from typing import Any

DEFAULT_IDS = "0x" + "0" * 63 + "5,0x" + "0" * 63 + "6"
DEFAULT_TYPE = (
    "0x0000000000000000000000000000000000000000000000000000000000000002"
    "::timelock::TimeLock"
    "<0x0000000000000000000000000000000000000000000000000000000000000002"
    "::balance::Balance"
    "<0x0000000000000000000000000000000000000000000000000000000000000002"
    "::iota::IOTA>>"
)
DEFAULT_OWNER = "0x8f84b5cbe97d9f0603f695e3ae7630fb288dc07567ab1788cc97d67ca149ead0"

DEFAULT_VERSION_PIN_ADDRESS = "0x" + "0" * 63 + "6"  # Clock — most-versioned object on staging
DEFAULT_VERSION_LOOKBACKS = "0,100,1000,10000,100000,1000000,2000000"

# `dynamic-fields` shape uses the synthetic `owner(address, rootVersion)`
# entry point, so the parent address does NOT need to be a top-level
# addressable object — we can target an owner-only address that has DFs in
# `objects_backward_history`. The default below is the wrapped inner
# `Versioned<RandomInner>` of Random on staging (~225k DF versions).
DEFAULT_DF_PARENT_ADDRESS = "0x405584e5568824afea966c0bed59e65f571d013cc8c3c5e4d616d173717d51b3"
DEFAULT_DF_LATEST_VERSION = 2533836  # max DF version observed for the parent above
DEFAULT_DF_LOOKBACKS = "0,100,1000,10000,100000,200000"

CURSOR_OBJECT_ID = bytes(32)


def encode_cursor(object_id: bytes, checkpoint_viewed_at: int) -> str:
    """Encode `HistoricalObjectCursor { o: Vec<u8>, c: u64 }` as BCS+base64.

    BCS layout: ULEB128(len(o)) || o || u64_le(c).
    """
    if len(object_id) > 127:
        raise ValueError("object_id length must fit in single-byte ULEB128")
    payload = bytes([len(object_id)]) + object_id + struct.pack("<Q", checkpoint_viewed_at)
    return base64.b64encode(payload).decode("ascii")


LATEST_QUERY = "query { checkpoint { sequenceNumber } }"

LATEST_VERSION_QUERY = """
query Q($addr: IotaAddress!) {
  object(address: $addr) { version }
}
"""

VERSION_PIN_QUERY = """
query Q($addr: IotaAddress!, $v: UInt53!) {
  object(address: $addr, version: $v) { address version }
}
"""

OBJECT_KEYS_QUERY = """
query Q($addr: IotaAddress!, $v: UInt53!) {
  objects(filter: { objectKeys: [{ objectId: $addr, version: $v }] }, first: 1) {
    nodes { address version }
    pageInfo { hasNextPage endCursor }
  }
}
"""

DYNAMIC_FIELDS_QUERY = """
query Q($addr: IotaAddress!, $v: UInt53!) {
  owner(address: $addr, rootVersion: $v) {
    dynamicFields(first: 50) {
      nodes { name { type { repr } } }
      pageInfo { hasNextPage }
    }
  }
}
"""


def build_query(shape: str, ids: list[str], type_str: str, owner: str, first: int) -> str:
    """Return a parametric GraphQL query for the given filter shape.

    All shapes accept a single `$after: String` variable; cursors are crafted
    on the client side to pin `checkpoint_viewed_at`.
    """
    if shape == "ids":
        id_list = ", ".join(f'"{i}"' for i in ids)
        filt = f"filter: {{ objectIds: [{id_list}] }}, "
    elif shape == "type":
        filt = f'filter: {{ type: "{type_str}" }}, '
    elif shape == "owner":
        filt = f'filter: {{ owner: "{owner}" }}, '
    elif shape == "empty":
        filt = ""
    else:
        raise ValueError(f"unknown shape: {shape}")
    return f"""
query Q($after: String) {{
  objects({filt}first: {first}, after: $after) {{
    nodes {{ address version }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}
"""


def post(url: str, query: str, variables: dict[str, Any] | None = None, timeout: float = 30.0) -> dict:
    body = json.dumps({"query": query, "variables": variables or {}}).encode()
    req = urllib.request.Request(
        url, data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


def probe_latest(url: str) -> int:
    resp = post(url, LATEST_QUERY)
    if "errors" in resp:
        sys.exit(f"latest-cp probe failed: {resp['errors']}")
    return int(resp["data"]["checkpoint"]["sequenceNumber"])


def probe_latest_version(url: str, address: str) -> int:
    resp = post(url, LATEST_VERSION_QUERY, {"addr": address})
    if "errors" in resp:
        sys.exit(f"latest-version probe failed for {address}: {resp['errors']}")
    obj = resp["data"].get("object")
    if obj is None:
        sys.exit(f"object {address} not found")
    return int(obj["version"])


def percentile(values: list[float], p: float) -> float:
    if not values:
        return float("nan")
    s = sorted(values)
    idx = min(len(s) - 1, int(len(s) * p))
    return s[idx]


def run_shape(
    shape: str,
    url: str,
    query: str,
    lookbacks: list[int],
    samples: int,
    warmup: int,
    latest_cp: int,
    csv_rows: list[str],
) -> dict[int, list[float]]:
    print(f"\n--- shape={shape} ---", file=sys.stderr)
    results: dict[int, list[float]] = {}
    for k in lookbacks:
        cursor_cp = latest_cp - k
        cursor = encode_cursor(CURSOR_OBJECT_ID, cursor_cp)
        latencies: list[float] = []
        for i in range(warmup + samples):
            t0 = time.perf_counter()
            resp = post(url, query, {"after": cursor})
            t1 = time.perf_counter()
            if "errors" in resp:
                sys.exit(f"query error shape={shape} K={k} sample {i}: {resp['errors']}")
            latency_ms = (t1 - t0) * 1000
            if i >= warmup:
                latencies.append(latency_ms)
                csv_rows.append(f"{shape},{k},{i - warmup},{latest_cp},{cursor_cp},{latency_ms:.3f}")
        results[k] = latencies
        p50 = statistics.median(latencies)
        p95 = percentile(latencies, 0.95)
        mx = max(latencies)
        mn = min(latencies)
        print(
            f"  K={k:4d}  n={len(latencies):>3d}  min={mn:7.1f}ms  "
            f"p50={p50:7.1f}ms  p95={p95:7.1f}ms  max={mx:7.1f}ms",
            file=sys.stderr,
        )
    return results


def run_version_sweep(
    shape: str,
    url: str,
    query: str,
    address: str,
    version_lookbacks: list[int],
    samples: int,
    warmup: int,
    csv_rows: list[str],
    explicit_latest_version: int | None = None,
) -> tuple[int, dict[int, list[float]]]:
    """Sweep `version=latest_version - K` for K in version_lookbacks.

    `K` means "versions back from latest". Used by `version-pin` (legacy
    `objects_history` path), `object-keys` (new `historical.rs` path) and
    `dynamic-fields` (new `dynamic_fields.rs` path via synthetic `owner`).

    `explicit_latest_version` skips the `object(address).version` probe for
    addresses that aren't top-level addressable (used by `dynamic-fields`).
    """
    print(f"\n--- shape={shape} (address={address}) ---", file=sys.stderr)
    latest_version = explicit_latest_version if explicit_latest_version is not None else probe_latest_version(url, address)
    print(f"  latest_version = {latest_version}", file=sys.stderr)
    results: dict[int, list[float]] = {}
    for k in version_lookbacks:
        if k > latest_version - 1:
            print(f"  K={k} skipped (object has only {latest_version} versions)", file=sys.stderr)
            continue
        v = latest_version - k
        latencies: list[float] = []
        for i in range(warmup + samples):
            t0 = time.perf_counter()
            resp = post(url, query, {"addr": address, "v": v})
            t1 = time.perf_counter()
            if "errors" in resp:
                sys.exit(f"query error shape={shape} K={k} sample {i}: {resp['errors']}")
            latency_ms = (t1 - t0) * 1000
            if i >= warmup:
                latencies.append(latency_ms)
                csv_rows.append(f"{shape},{k},{i - warmup},{latest_version},{v},{latency_ms:.3f}")
        results[k] = latencies
        print(
            f"  K={k:8d}  v={v:>10d}  n={len(latencies):>3d}  "
            f"min={min(latencies):7.1f}ms  p50={statistics.median(latencies):7.1f}ms  "
            f"p95={percentile(latencies, 0.95):7.1f}ms  max={max(latencies):7.1f}ms",
            file=sys.stderr,
        )
    return latest_version, results


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--url", default="http://localhost:9125/graphql",
        help="GraphQL endpoint (default: http://localhost:9125/graphql)",
    )
    ap.add_argument(
        "--shape", default="all",
        choices=["ids", "type", "owner", "empty", "version-pin", "object-keys",
                 "dynamic-fields", "all"],
        help="Filter shape to test (default: all)",
    )
    ap.add_argument(
        "--ids", default=DEFAULT_IDS,
        help=f"Comma-separated object IDs for shape=ids (default: {DEFAULT_IDS})",
    )
    ap.add_argument(
        "--type", dest="type_str", default=DEFAULT_TYPE,
        help="Object type for shape=type (default: heaviest type on staging)",
    )
    ap.add_argument(
        "--owner", default=DEFAULT_OWNER,
        help="Owner address for shape=owner (default: heaviest owner on staging)",
    )
    ap.add_argument(
        "--version-pin-address", default=DEFAULT_VERSION_PIN_ADDRESS,
        help="Object address for shape=version-pin (default: Clock 0x..06)",
    )
    ap.add_argument(
        "--version-lookbacks", default=DEFAULT_VERSION_LOOKBACKS,
        help="Comma-separated version-recency distances for version-pin / object-keys shapes",
    )
    ap.add_argument(
        "--df-parent-address", default=DEFAULT_DF_PARENT_ADDRESS,
        help=f"Parent owner address for shape=dynamic-fields (default: {DEFAULT_DF_PARENT_ADDRESS} — wrapped Versioned of Random on staging)",
    )
    ap.add_argument(
        "--df-latest-version", type=int, default=DEFAULT_DF_LATEST_VERSION,
        help=f"Max valid rootVersion to sweep from for shape=dynamic-fields (default: {DEFAULT_DF_LATEST_VERSION})",
    )
    ap.add_argument(
        "--df-lookbacks", default=DEFAULT_DF_LOOKBACKS,
        help="Comma-separated version-recency distances for shape=dynamic-fields",
    )
    ap.add_argument(
        "--first", type=int, default=50,
        help="Page size for type/owner/empty shapes (default: 50; ids shape always uses len(ids))",
    )
    ap.add_argument(
        "--lookbacks", default="0,100,200,400,600,800,899",
        help="Comma-separated lookback distances K (cps) to test",
    )
    ap.add_argument(
        "--samples", type=int, default=20,
        help="Number of samples per lookback bucket",
    )
    ap.add_argument(
        "--warmup", type=int, default=3,
        help="Warmup queries per bucket (not recorded)",
    )
    ap.add_argument("--csv", type=str, default=None,
                    help="Optional path to write raw per-sample CSV")
    args = ap.parse_args()

    lookbacks = [int(x) for x in args.lookbacks.split(",") if x.strip()]
    version_lookbacks = [int(x) for x in args.version_lookbacks.split(",") if x.strip()]
    df_lookbacks = [int(x) for x in args.df_lookbacks.split(",") if x.strip()]
    ids = [x.strip() for x in args.ids.split(",") if x.strip()]
    shapes = (
        ["ids", "type", "owner", "empty", "version-pin", "object-keys", "dynamic-fields"]
        if args.shape == "all"
        else [args.shape]
    )

    latest = probe_latest(args.url)
    print(f"latest_cp = {latest}", file=sys.stderr)
    print(f"cursor sweep K = {lookbacks}", file=sys.stderr)
    print(f"version-pin sweep K = {version_lookbacks}", file=sys.stderr)
    print(f"shapes = {shapes}", file=sys.stderr)
    print(f"{args.warmup} warmup + {args.samples} measured samples per (shape, K)", file=sys.stderr)

    csv_rows: list[str] = ["shape,lookback_k,sample,anchor_cp_or_version,target_cp_or_version,latency_ms"]
    cursor_results: dict[str, dict[int, list[float]]] = {}
    version_sweep_results: dict[str, tuple[int, dict[int, list[float]]]] = {}
    for shape in shapes:
        if shape == "version-pin":
            version_sweep_results[shape] = run_version_sweep(
                shape, args.url, VERSION_PIN_QUERY,
                args.version_pin_address, version_lookbacks,
                args.samples, args.warmup, csv_rows,
            )
        elif shape == "object-keys":
            version_sweep_results[shape] = run_version_sweep(
                shape, args.url, OBJECT_KEYS_QUERY,
                args.version_pin_address, version_lookbacks,
                args.samples, args.warmup, csv_rows,
            )
        elif shape == "dynamic-fields":
            version_sweep_results[shape] = run_version_sweep(
                shape, args.url, DYNAMIC_FIELDS_QUERY,
                args.df_parent_address, df_lookbacks,
                args.samples, args.warmup, csv_rows,
                explicit_latest_version=args.df_latest_version,
            )
        else:
            first_n = len(ids) if shape == "ids" else args.first
            query = build_query(shape, ids, args.type_str, args.owner, first_n)
            cursor_results[shape] = run_shape(
                shape, args.url, query, lookbacks, args.samples, args.warmup, latest, csv_rows
            )

    if args.csv:
        with open(args.csv, "w") as f:
            f.write("\n".join(csv_rows) + "\n")
        print(f"\nraw CSV: {args.csv}", file=sys.stderr)

    print()
    print("| shape | K | n | min (ms) | p50 (ms) | p95 (ms) | max (ms) |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    for shape in shapes:
        if shape in version_sweep_results:
            continue
        for k in lookbacks:
            lats = cursor_results[shape][k]
            print(
                f"| {shape} | {k} | {len(lats)} | {min(lats):.1f} | "
                f"{statistics.median(lats):.1f} | {percentile(lats, 0.95):.1f} | {max(lats):.1f} |"
            )
    for shape, (latest_version, vp_results) in version_sweep_results.items():
        addr = args.df_parent_address if shape == "dynamic-fields" else args.version_pin_address
        print()
        print(f"| {shape} (address={addr}, latest_v={latest_version}) | K versions back | n | min (ms) | p50 (ms) | p95 (ms) | max (ms) |")
        print("|---|---:|---:|---:|---:|---:|---:|")
        for k in sorted(vp_results):
            lats = vp_results[k]
            print(
                f"| {shape} | {k} | {len(lats)} | {min(lats):.1f} | "
                f"{statistics.median(lats):.1f} | {percentile(lats, 0.95):.1f} | {max(lats):.1f} |"
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
