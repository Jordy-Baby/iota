# Local Move Debugging, Tracing & Gas Profiling

A key advantage of the local executor over remote dry-run / dev-inspect is the ability to capture debug output and execution traces that nodes never expose. This document describes the workflow, the underlying VM capabilities it builds on, and the phased rollout.

## Workflow

The developer experience this unlocks:

1. Write a Move package locally (source, not published) and add `debug::print` statements where useful.
2. Compile it in-process via `LocalPackage::compile` and give it a caller-chosen **synthetic `ObjectID`** (e.g. `0x42`). No network publish is required.
3. Install the compiled package into the executor's `BackingPackageStore` under that synthetic ID.
4. Build a transaction that references `0x42::<module>::<fn>` — this works for plain PTB calls as well as for a `MoveAuthenticator` whose `function_ref` points at the synthetic package.
5. Run `simulate_transaction_with_debug` (or `simulate_signed_transaction_with_debug` for the authenticator path) with a `DebugConfig` that independently toggles:
   - `debug::print` output capture,
   - Speedscope-format gas profile,
   - Instruction-level `MoveTrace`.
6. Iterate on the Move source until gas and behavior are satisfactory, then publish.

Because the authenticator's Move call is metered and traced identically to regular PTB execution, the same flow profiles the **`MoveAuthenticator`** function — which is exactly the workflow needed to gas-optimize a custom authenticator before deployment.

## Building blocks

### 1. Move `debug::print` output

**Current state.** The Move VM executor is created with `silent=true` at [execution.rs:66](../src/execution.rs#L66), which replaces `debug::print` / `debug::print_stack_trace` native functions with no-ops. The actual print logic is behind a `testing` feature gate in `move-stdlib-natives`.

**What changes.**
- `ExecutionEnv` gains a `debug_config: DebugConfig` field; `DebugConfig::capture_debug_prints` flips the `silent` argument on `iota_execution::executor(...)`.
- Each executor gains `with_debug(DebugConfig)` and `simulate_transaction_with_debug(...)` methods.

**Phase 1 output behavior.** `debug::print` writes to stdout via `println!("[debug] {value}")`. Output is visible in the terminal and captureable via `gag::BufferRedirect` in tests.

**Phase 2 — structured capture.** Modify the Move `native_print` implementation in `external-crates/move/crates/move-stdlib-natives/src/debug.rs` to push into a thread-local sink when installed. `DebugConfig::structured_debug_capture` installs the sink and `DebugArtifacts::debug_prints` returns the captured lines.

**Benefit.** Bridges the gap between `iota move test` (where debug prints work) and real transaction simulation — impossible with remote dry-run.

### 2. Gas profiling

**Current state.** `iota_execution::executor()` accepts `enable_profiler: Option<PathBuf>`; when set, the Move VM writes a Speedscope-format JSON gas profile to that path. `iota-local-executor` currently passes `None`.

**What changes.**
- `DebugConfig::profile: Option<ProfileSink>` with `ProfileSink = File(PathBuf) | Capture`. `File` forwards the path straight into `enable_profiler`; `Capture` points the profiler at a temp file and reads the JSON back into `DebugArtifacts::profile` as `ProfileOutput::Json(Vec<u8>)`.

**Output.** A Speedscope JSON profile that can be visualized with `speedscope` or the existing `move-gas-profiler` tool, showing per-instruction gas costs, function call stacks, and hotspots.

**Benefit.** Gas optimization workflows run entirely locally — including for custom Move authenticators, where today only on-chain measurements are possible.

### 3. Instruction-level tracing

**Current state.** `move-trace-format::MoveTraceBuilder` captures instruction-level execution traces; `Executor::execute_transaction_to_effects` and `authenticate_then_execute_transaction_to_effects` both accept `trace_builder_opt: &mut Option<MoveTraceBuilder>`, but `Executor::dev_inspect_transaction` does not. The local executor only uses `dev_inspect_transaction` for the non-authenticator path, and hardcodes `&mut None` on the authenticator path at [execution.rs:352](../src/execution.rs#L352).

**What changes.**
- Extend `Executor::dev_inspect_transaction` upstream to take `trace_builder_opt: &mut Option<MoveTraceBuilder>`. Small, additive.
- Rewire the hardcoded `&mut None` in `execute_with_move_authenticator` to thread the caller-supplied trace builder.
- `DebugConfig::trace = true` constructs a fresh `MoveTraceBuilder` per simulation and returns the resulting `MoveTrace` in `DebugArtifacts::trace`.

**Benefit.** The full structured trace is available programmatically for post-processing — the foundation for future IDE integration (Phase 3).

## Synthetic-package workflow

`LocalPackage` (in `src/local_package.rs`) compiles a Move source package and produces an on-chain `MovePackage` with a caller-chosen `ObjectID`:

1. Register IOTA package hooks.
2. Build a `BuildConfig` that maps the package's declared named address to the synthetic ID (via `new_for_testing_replace_addresses`). The compiled bytecode embeds the synthetic ID wherever the source writes `my_pkg::…`.
3. Run `BuildConfig::build(path)` → `CompiledPackage`.
4. Serialize each `CompiledModule` into the `module_map`.
5. Construct `linkage_table` (one `UpgradeInfo` per framework dep) and `type_origin_table` (one `TypeOrigin` per struct/enum declared in the package, with `package = synthetic_id`).
6. `MovePackage::new(synthetic_id, SequenceNumber::from(1), module_map, max_move_package_size, type_origins, linkage_table)`.
7. Wrap in `Object::new_package_from_data(...)` and expose `install_into(&InMemoryStore)` / `install_into_caching(&CachingStore)`.

Framework packages (`0x2`, `0x3`, …) continue to come from `BuiltInFramework::genesis_objects()` — `LocalPackage` does not touch them.

`CachingStore` gets a small `RwLock<BTreeMap<ObjectID, PackageObject>>` override checked before any remote fetch, so mixed workflows (synthetic local package + remotely-fetched input objects) work unchanged.

## API surface

```rust
pub struct DebugConfig {
    pub capture_debug_prints: bool,
    pub profile: Option<ProfileSink>,
    pub trace: bool,
    pub structured_debug_capture: bool, // Phase 2
}

pub enum ProfileSink { File(PathBuf), Capture }
pub enum ProfileOutput { Path(PathBuf), Json(Vec<u8>) }

pub struct DebugArtifacts {
    pub debug_prints: Vec<String>,  // empty in Phase 1
    pub profile: Option<ProfileOutput>,
    pub trace: Option<move_trace_format::format::MoveTrace>,
}

pub struct DebugSimulateResult {
    pub result: SimulateTransactionResult,
    pub artifacts: DebugArtifacts,
}
```

Each executor gets:

```rust
pub fn with_debug(self, debug: DebugConfig) -> Self;
pub fn simulate_transaction_with_debug(&self, tx: TransactionData, checks: VmChecks)
    -> Result<DebugSimulateResult>;
pub fn simulate_signed_transaction_with_debug(&self, signed: SenderSignedData, checks: VmChecks)
    -> Result<DebugSimulateResult>;
```

The existing non-debug methods stay unchanged — all new APIs are additive.

## Phased rollout

**Phase 1 — core feature.** `DebugConfig`, `DebugArtifacts`, `DebugSimulateResult`, `LocalPackage`, `with_debug` builders, trace-builder rewiring, `dev_inspect_transaction` trait extension, `CachingStore::insert_package_override`, Move fixture package, tests, example. Debug prints go to stdout; `debug_prints` stays empty until Phase 2.

**Phase 2 — structured debug capture.** Modify `move-stdlib-natives/src/debug.rs` to push into a thread-local sink when installed. Populates `DebugArtifacts::debug_prints`. Crosses into `external-crates/move/` — call out scope at review.

**Phase 3 — IDE hook.** Expose `trait DebugObserver` attached via `DebugConfig`. Implementation walks the returned `MoveTrace` and correlates frame pc + module to `CompiledPackage.source_maps`, giving per-line gas and per-call trace jumps. No VM cost — pure post-processing. Additive over Phase 1's `MoveTrace`.

## Notes

- **No on-chain version drift.** `LocalPackage::compile` takes `&ProtocolConfig` explicitly so the framework version it links against matches the executor's protocol version.
- **Semantics-preserving.** An effects-parity test gates the feature: running the same transaction with `DebugConfig::default()` vs. fully-enabled debug must produce byte-identical `TransactionEffects`.
- **Authenticator parity.** Because the authenticator's Move call flows through the same VM as any other PTB call, `profile` and `trace` cover it for free. The only code change needed is replacing the hardcoded `&mut None` trace builder in the authenticator execution path.
