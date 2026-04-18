# Gas-profiling a Move authenticator function

This example produces a speedscope flamegraph for a Move `#[authenticator]`
function using only the tools that ship today — no changes to the IOTA
codebase are required.

The test in [sources/authenticator.move](sources/authenticator.move) invokes
`authenticate_hello_world` directly as a regular Move call from a `#[test]`.
When the `iota` binary is built with the `tracing` feature and the
`MOVE_VM_PROFILE` environment variable is set, the Move VM attaches a gas
profiler to every test and writes a speedscope JSON file per run.

## 1. Build `iota` with the `tracing` feature

The Move VM profiler is gated behind the `tracing` feature. Stock binaries
don't have it; build from source once:

```sh
cargo build --release --features tracing --bin iota
```

The resulting binary is at `<repo>/target/release/iota` (shared between
`iota/` and `iota-rust-sdk/` via the symlinked `target/` directory).

## 2. Run the profiled test

From this directory, with the tracing-enabled `iota` binary on PATH:

```sh
MOVE_VM_PROFILE=1 iota move test profile_authenticate_hello_world --threads 1
```

Or run the repo's binary directly without installing:

```sh
MOVE_VM_PROFILE=1 \
  cargo run --release --features tracing --bin iota --manifest-path ../../../Cargo.toml -- \
  move test --path . profile_authenticate_hello_world --threads 1
```

What each flag does:

- `MOVE_VM_PROFILE=1` — any non-empty value enables the profiler. The env
  var's **presence** is what activates it; the value is ignored.
- `profile_authenticate_hello_world` — positional filter. Every matched
  test produces its own JSON file, so narrowing the filter keeps the
  output set small.
- `--threads 1` — required. The profiler uses thread-locals; parallel test
  execution interleaves frames across threads and produces unusable output.

## 3. Locate the output

The profiler writes one file per test invocation to the **current working
directory**, with this name format:

```
gas_profile.json_<test_function_name>_<unix_nanos>.json
```

For this example:

```
gas_profile.json_profile_authenticate_hello_world_<timestamp>.json
```

The timestamp suffix means repeated runs do not overwrite previous profiles.

## 4. Normalize the event times

The unit-test harness has a known bug where the profiler's event timestamps
are computed against a mismatched `start_gas`, placing the first real frame
at a value near `u64::MAX`. speedscope then renders the `root` frame
spanning the full range and every other frame gets compressed to zero
width — which is why a raw profile appears to contain only `root`.

Run this one-liner from the directory containing the JSON to produce a
normalized copy:

```sh
python3 -c "
import json, sys, glob
src = glob.glob('gas_profile.json_*.json')[-1]
with open(src) as f: d = json.load(f)
p = d['profiles'][0]
first = p['events'][1]['at']
for e in p['events']:
    if e['at'] != 0: e['at'] -= first
p['endValue'] -= first
out = src.replace('.json_', '_normalized.json_')
with open(out, 'w') as f: json.dump(d, f, indent=2)
print(out)
"
```

It prints the path of the normalized file.

## 5. Visualize the flamegraph

Open https://www.speedscope.app and drag the normalized JSON onto the page.

Three useful views:

- **Time Order** — every frame entered, in call order.
- **Left Heavy** — identical stacks merged; best for spotting hotspots.
- **Sandwich** — per-function totals with callers and callees.

The horizontal axis is gas units, not wall-clock time. Numbers are
deterministic for a fixed Move cost schedule and comparable across runs of
the same package built against the same framework version.

## Caveats

- **Direct call, not dispatch.** The test invokes the authenticator as a
  plain Move function. Gas spent on `#[authenticator]` dispatch (signature
  verification, function lookup, etc.) during a real transaction flow is
  not captured here.
- **Empty `AuthContext`.** `auth_context::new_with_tx_inputs` is called
  with empty vectors for `tx_inputs`, `tx_commands`, and `tx_data_bytes`.
  An authenticator that actually reads those would need real data plumbed
  in — see `iota::auth_context` for the accessors.
- **`tracing` feature cost.** Every gas charge goes through an extra
  indirection when profiling is compiled in. If you also care about the
  absolute gas totals, compare against a non-`tracing` build.
