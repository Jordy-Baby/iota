# `MoveAuthenticator` verification

Fully validate a `MoveAuthenticator` signature locally by running its authenticator function inside the Move VM — the only way to check these signatures short of submitting the transaction.

## What a `MoveAuthenticator` is

Instead of an Ed25519/Secp256k1 signature, the sender supplies a `MoveAuthenticator` that points at an `authenticator_function` on an on-chain "abstract account" object. At execution time the VM calls that function with the signature's args + the transaction bytes + the account object; if the function aborts, the transaction is rejected.

Standard schemes (Ed25519, Secp256k1/r1, MultiSig) are verified cryptographically before execution. **A `MoveAuthenticator` can't be**: its acceptance rule is code. Only full local simulation reveals whether it'll succeed.

## End-to-end

```rust
use iota_local_executor::{JsonRpcExecutor, SenderSignedData, VmChecks};
use iota_types::{
    move_authenticator::MoveAuthenticator,
    signature::GenericSignature,
    transaction::{CallArg, ObjectArg, TransactionData},
};

// 1. Build the transaction data as usual.
let tx_data: TransactionData = /* … */;

// 2. Build a `MoveAuthenticator`. For `authenticate_free_access` (no extra
//    args), `signature_args` and `receiving_objects` are both empty; the
//    `self_call_arg` references the abstract-account shared object.
let self_call_arg = CallArg::Object(ObjectArg::SharedObject {
    id: aa_ref.0,
    initial_shared_version: aa_ref.1,
    mutable: false,
});
let move_auth = GenericSignature::MoveAuthenticator(MoveAuthenticator::new_v1(
    vec![],          // signature-function arg `CallArg`s
    vec![],          // receiving objects
    self_call_arg,
));

let signed = SenderSignedData::new(tx_data, vec![move_auth]);

// 3. Simulate with signature verification on.
let executor = JsonRpcExecutor::new(client).await?;
let result = executor
    .simulate_signed_transaction(signed, VmChecks::Disabled)
    .await?;

if result.effects.status().is_ok() {
    // Authenticator accepted — your signature would have been valid on-chain.
} else {
    // Authenticator rejected or transaction body aborted.
}
```

## When signatures are checked

- **Standard schemes** — full cryptographic check runs **before** execution. A failure there returns an error from `simulate_signed_transaction` without running the VM.
- **`MoveAuthenticator`** — sender address is pre-checked (so trivially wrong pairings fail fast), then the authenticator's Move call runs inside the VM. Its result shows up in `result.effects.status()`. A function abort becomes an execution error, not a pre-execution error.

## Debugging / profiling the authenticator function

Because the authenticator is just a Move call, every debugging feature works on it. Enable `DebugConfig::profile + trace` to profile its gas cost, and the resulting Speedscope profile will include frames for the authenticator function alongside the PTB body.

See [authenticator-gas-optimization.md](details/authenticator-gas-optimization.md) for that workflow.

## Examples and tests

- [../tests/e2e_executor_comparison.rs::simulate_signed_transaction_move_authenticator_valid](../tests/e2e_executor_comparison.rs) — happy path, `authenticate_free_access`
- [../tests/e2e_executor_comparison.rs::simulate_signed_transaction_move_authenticator_invalid_args](../tests/e2e_executor_comparison.rs) — bogus Ed25519 signature rejected by `authenticate_ed25519`
