# Gas profiling

Capture a Speedscope-format gas profile of every Move instruction your transaction executes, then visualise it in https://www.speedscope.app/ to see per-frame hotspots and call stacks.

Prerequisite: the code you want to profile must live in a local Move package installed into the executor. See [local-packages.md](local-packages.md) for how to compile and install one.

## Configure the executor

Profiling is opt-in via `DebugConfig`. Two sinks are available:

- **`ProfileSink::File(path)`** — the Move VM writes the Speedscope JSON to a filesystem path you supply. Lowest overhead; useful when you want to keep the profile for later.
- **`ProfileSink::Capture`** — the executor writes into a temp dir it manages and reads the JSON back into memory after execution. The result lands in `DebugArtifacts::profile` as `ProfileOutput::Json(Vec<u8>)`.

```rust
use iota_local_executor::{
    DebugConfig, InMemoryStore, LocalPackage, OfflineExecutor, ProfileOutput, ProfileSink,
    VmChecks,
};

// (... seed store + install local package as in local-packages.md ...)

let executor = OfflineExecutor::with_debug(
    ProtocolVersion::MAX,
    1000,
    0,
    0,
    store,
    DebugConfig {
        profile: Some(ProfileSink::Capture),
        ..DebugConfig::default()
    },
)?;
```

## Run the transaction

Use `simulate_transaction_with_debug` (or the `_signed_` variant) instead of `simulate_transaction` — the `_with_debug` form returns a `DebugSimulateResult` that carries both the normal result and the captured artifacts.

```rust
let out = executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
assert!(out.result.effects.status().is_success());

let profile = out.artifacts.profile.expect("ProfileSink::Capture was set");
let bytes = match profile {
    ProfileOutput::Json(b) => b,
    ProfileOutput::Path(p) => std::fs::read(p)?,
};
```

## Consume the profile

### Option A — open it in Speedscope (GUI)

```rust
let out_path = std::env::temp_dir().join("gas_profile.json");
std::fs::write(&out_path, &bytes)?;
println!("View locally: npx speedscope {}", out_path.display());
println!("Or upload to: https://www.speedscope.app/");
```

Two ways to view the same file:

- **Locally**: `npx speedscope <path>` (requires Node) runs the bundled Speedscope UI on `localhost` — no upload, nothing leaves your machine, works offline.
- **In the browser**: drag the file onto https://www.speedscope.app/. It also runs in-browser (the file doesn't leave your machine), but needs network for the initial page load.

Either way you get three views: time-order, left-heavy, and sandwich. Each frame's "weight" is gas consumed.

### Option B — parse it programmatically

The JSON is standard Speedscope. Walk `shared.frames[]` to find which Move functions were called (the `file` field contains the fully-qualified `<pkg-id>::<module>::<fn>`), and `profiles[0].events[]` for the gas-timeline:

```rust
let json: serde_json::Value = serde_json::from_slice(&bytes)?;
let frames = json.pointer("/shared/frames").unwrap().as_array().unwrap();
for frame in frames {
    println!("{} at {}", frame["name"], frame["file"]);
}
```

## Multi-session profiles (authenticators)

A transaction with a `MoveAuthenticator` actually runs the Move VM twice — once for the authenticator, once for the PTB body — and each writes its own Speedscope file. The executor **merges them automatically** into one document: `shared.frames` is the deduplicated union, and `profiles[]` contains one profile per VM invocation. You'll see both the authenticator function's frames and the PTB body's frames in the same view.

## Opting out — `ProfileSink::File` for persistence

```rust
let out_dir = std::path::PathBuf::from("/tmp/gas-profiles");
std::fs::create_dir_all(&out_dir)?;

let executor = OfflineExecutor::with_debug(
    /* … */,
    DebugConfig {
        profile: Some(ProfileSink::File(out_dir.join("run.json"))),
        ..DebugConfig::default()
    },
)?;

executor.simulate_transaction_with_debug(tx, VmChecks::Disabled)?;
// Files now live under /tmp/gas-profiles/run_<name>_<timestamp>.json.
```

The VM profiler appends `_<frame-name>_<timestamp>.<ext>` to the path you pass, so the actual filenames won't match your input exactly.

## Examples and tests

- [../examples/local_package_debug.rs](../examples/local_package_debug.rs) — writes a profile to `/tmp/iota-local-executor-demo-profile.json`
- [../tests/local_package.rs::profile_captures_fixture_module_frame](../tests/local_package.rs) — asserts a frame for `::hello::` shows up
- [../tests/e2e_executor_comparison.rs::simulate_signed_transaction_move_authenticator_with_debug](../tests/e2e_executor_comparison.rs) — multi-session (authenticator + body) profile merging
