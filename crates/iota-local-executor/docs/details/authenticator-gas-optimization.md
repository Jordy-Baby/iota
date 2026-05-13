# Authenticator gas-optimisation

Profile the gas cost of a `MoveAuthenticator`'s authenticator function locally and iterate on its Move code until it's cheap enough to ship. Today the only alternative is submitting transactions and reading their on-chain gas costs — which means paying real gas for every iteration of a signature scheme that hasn't stabilised yet.

Prerequisites:

- A working `MoveAuthenticator` flow — see [move-authenticator.md](../move-authenticator.md).
- Gas profiling basics — see [gas-profiling.md](../gas-profiling.md).

## Workflow

Combine the two: enable profiling + tracing on the executor, then run `simulate_signed_transaction_with_debug`. The authenticator function and the PTB body each run through the VM, and the returned profile covers both.

```rust
use iota_local_executor::{DebugConfig, GrpcExecutor, ProfileOutput, ProfileSink, VmChecks};

let executor = GrpcExecutor::with_debug(
    client,
    DebugConfig {
        profile: Some(ProfileSink::Capture),
        trace: true,
        ..DebugConfig::default()
    },
)
.await?;

let out = executor
    .simulate_signed_transaction_with_debug(signed_data, VmChecks::Disabled)
    .await?;

// The profile now contains frames for BOTH the authenticator call and the
// PTB body. The executor merged them into one Speedscope document so you
// see the full picture in a single view.
let profile = out.artifacts.profile.expect("ProfileSink::Capture was set");
if let ProfileOutput::Json(bytes) = profile {
    std::fs::write("/tmp/auth_profile.json", &bytes)?;
    println!("View locally: npx speedscope /tmp/auth_profile.json");
    println!("Or upload to: https://www.speedscope.app/");
}
```

## What to look for in the profile

The authenticator function's frames show up with their fully-qualified path in the `file` field, e.g. `0x...::abstract_account_keyed::authenticate_free_access`. In Speedscope's "sandwich" view, sort by "self" weight and look for:

- **Unnecessary crypto work** — redundant BCS re-encoding, double-hashing, Merkle recomputation.
- **Hot paths in helpers** — an `authenticate_ed25519` that inlines a full signature check every call vs. precomputing something once at account creation.
- **Dynamic-field reads** — dynamic-field access is one of the more expensive Move VM operations; batching or avoiding re-reads is a classic win.

Cross-reference with the `DebugArtifacts::trace` (instruction-level) if the frame-level view isn't specific enough — but usually the function-level profile is enough to find the optimisation.

## Iteration loop

1. Run the signed tx through `simulate_signed_transaction_with_debug`.
2. Note the `net_gas_usage` ([gas-estimation.md](../gas-estimation.md)) and the authenticator's frame self-weight in the profile.
3. Change the Move code.
4. Recompile the [local package](../local-packages.md) and run again. Goto 1.

Because everything is local there's no chain state drift between iterations — the "before" and "after" numbers are directly comparable. (Submit once at the end to validate against the real chain.)

## Examples and tests

- [../../tests/e2e_executor_comparison.rs::simulate_signed_transaction_move_authenticator_with_debug](../../tests/e2e_executor_comparison.rs) — asserts the merged profile contains a frame naming `authenticate_free_access` (the authenticator) alongside the PTB body frames
