# iota-local-vm-wasm

The full IOTA Move VM (`iota-execution`, `iota-types`, `iota-framework`,
`move-vm-runtime`, …) compiled to `wasm32-unknown-unknown` and exposed
to JavaScript. Runs the same `dev_inspect_transaction` /
`authenticate_then_execute_transaction_to_effects` paths the node uses —
no network call, no signing, no submission.

The crate began life as a stake-only inspector and has since grown into
a generic local VM for the browser; the two demo flows reflect that:

- **Unsigned**: paste a base-64 BCS-encoded `TransactionData`, pick a
  network, and the wasm side runs it through `dev_inspect` against the
  network's current objects.
- **Signed (incl. `MoveAuthenticator`)**: also provide the wallet's raw
  signature blob(s). The wasm side verifies standard schemes
  cryptographically and runs the `MoveAuthenticator`'s function inside
  the Move VM. Wallets can use this to fully check a `MoveAuthenticator`
  signature **without sending it to a node** — the original motivation
  for the crate.

The demo page (`web/index.html`) wires up:

1. A free-form input area for `TransactionData` + optional signatures (one
   raw signature blob per line, base-64).
2. A network picker (`mainnet` / `testnet` / `devnet` / `localnet`) that
   tells the JS shim where to fetch input objects from.
3. Three sample buttons — a devnet staking transaction, plus two pre-baked
   `MoveAuthenticator` fixtures (happy path + an invalid-signature failure)
   captured from a real `iota-local-executor` integration test. The
   fixture path ships the full set of objects with the JSON file, so those
   two samples work entirely offline.

## Build

```sh
# from the repo root
rustup target add wasm32-unknown-unknown            # one-time
cargo install wasm-pack --locked                    # one-time

wasm-pack build crates/iota-local-vm-wasm \
    --target web --release --no-typescript
```

The package is written to `crates/iota-local-vm-wasm/pkg/`.

## Run

Any static HTTP server pointed at `crates/iota-local-vm-wasm/` works.
The web page expects to reach `pkg/iota_local_vm_wasm.js` one level up
from `web/`.

```sh
cd crates/iota-local-vm-wasm
python3 -m http.server 8765
# open http://127.0.0.1:8765/web/
```

Click **Sample: stake tx (devnet)** for the original unsigned-flow demo,
or one of the **Sample: MoveAuthenticator** buttons to exercise the
signed-flow with bundled objects.

## WASM API

The exported wasm functions (`pkg/iota_local_vm_wasm.js`) are:

| Function                                                                                                                 | Purpose                                                                                                                                                                                                                                                         |
| ------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `decode_transaction(tx_b64)`                                                                                             | Decode a base-64 `TransactionData` and return its sender, gas, and the set of object IDs JS must fetch to feed the simulator.                                                                                                                                   |
| `decode_move_authenticator_objects(sig_b64)`                                                                             | For a `MoveAuthenticator` signature, return the extra object IDs JS must fetch (the authenticator's input objects + the `AuthenticatorFunctionRefV1` dynamic field on the account object). Returns `null` for any other signature scheme.                       |
| `derive_field_id(parent_id, key_type, key_bcs, is_dof)`                                                                  | Derive a `Field<K,V>`'s on-chain ID — used by JS to walk dynamic-field trees from GraphQL's `dynamicFields` connection.                                                                                                                                         |
| `simulate({ tx_b64, protocol_version, reference_gas_price, epoch_id, epoch_timestamp_ms, objects, strict, signatures })` | Run the transaction. `signatures` is optional; when non-empty the simulator runs `simulate_signed_transaction`, verifying crypto first (and, for `MoveAuthenticator`, running the auth function inside the VM). The result includes `signature_verified: bool`. |

`signatures` is an array of base-64-encoded raw signature blobs — exactly
what the wallet holds for each signer, `[flag || …]`.

## Regenerating the `MoveAuthenticator` samples

The samples in `web/samples/` are produced by two existing tests in
`iota-local-executor`. Setting `IOTA_LOCAL_VM_WASM_FIXTURE_DIR` makes
those tests also persist their tx + signatures + cached objects as a
self-contained JSON fixture. To regenerate:

```sh
IOTA_LOCAL_VM_WASM_FIXTURE_DIR=$PWD/web/samples \
  cargo test -p iota-local-executor --test e2e_executor_comparison \
  -- --test-threads=1 \
     simulate_signed_transaction_move_authenticator_valid \
     simulate_signed_transaction_move_authenticator_invalid_args
```

Each test brings up a real test cluster, publishes the abstract-account
package, mints an AA, signs a tx, and writes the resulting fixture to
`$IOTA_LOCAL_VM_WASM_FIXTURE_DIR/<name>.json`. Built-in framework
packages are filtered out — the wasm side preloads those via
`InMemoryStore::with_framework`.

## What changed in the VM to make this compile

The Move VM and its supporting crates pull in a lot of node-side code
(networking, metrics, consensus, file IO) that isn't valid on
`wasm32-unknown-unknown`. The changes are all behind
`#[cfg(target_arch = "wasm32")]` / `target.'cfg(not(...))'.dependencies`,
so non-wasm builds are byte-identical:

- `iota-types`: moved `prometheus`, `tonic`, `anemo`,
  `iota-network-stack`, `starfish-config`, `fastcrypto-tbls` to
  non-wasm-only deps. Replaced `iota_network_stack::Multiaddr` with a
  string-wrapping stub for wasm (BCS-compatible). Replaced
  `prometheus::{IntCounter, IntCounterVec, Histogram}` with no-op stubs
  inside `iota_types::metrics`. Gated DKG / consensus / network-address
  helpers that aren't reachable from execution.
- `iota-common`: gated network/file modules; rewrote the `debug_fatal!`
  macro to drop the metrics call on wasm.
- `iota-config`: gated everything except `transaction_deny_config` and
  `verifier_signing_config` (the two modules `iota-transaction-checks`
  needs).
- `iota-adapter-latest` (the latest execution layer): moved `iota-metrics`
  to a non-wasm dep and stubbed the single `monitored_scope` call.

Everything else (`move-binary-format`, `move-vm-runtime`,
`move-vm-types`, `move-stdlib-natives`, `iota-execution`,
`iota-framework`, `iota-transaction-checks`) compiles to wasm
unchanged once the dep graph stops pulling in `mio`/`tokio-net`.

## Architecture

```text
┌─────────────────────────────────────────────┐
│   browser page (web/index.html)             │
│   ┌──────────────┐  ┌─────────────────────┐ │
│   │ network ▼    │  │ tx + signatures     │ │
│   └──────┬───────┘  └────────┬────────────┘ │
│          │                   │               │
│          ▼                   ▼               │
│   ┌──────────────────────────────────────┐  │
│   │  app.js                              │  │
│   │  - decode_transaction()      (wasm)  │  │
│   │  - decode_move_authenticator_        │  │
│   │      objects()               (wasm)  │  │
│   │  - GraphQL: fetch BCS objects        │  │
│   │  - GraphQL: walk dynamic fields      │  │
│   │  - GraphQL: chain info               │  │
│   │  - simulate(req)             (wasm)  │  │
│   └──────────────────────────────────────┘  │
│                  │                           │
│                  ▼                           │
│   ┌──────────────────────────────────────┐  │
│   │  iota_local_vm_wasm_bg.wasm          │  │
│   │  ┌─ InMemoryStore (framework +       │  │
│   │  │  fetched objects)                 │  │
│   │  └─ OfflineExecutor                  │  │
│   │     ├─ simulate_transaction          │  │
│   │     │   └─ dev_inspect               │  │
│   │     └─ simulate_signed_transaction   │  │
│   │         ├─ verify_sender_signed_     │  │
│   │         │     data_message_          │  │
│   │         │     signatures             │  │
│   │         └─ authenticate_then_        │  │
│   │             execute_transaction_     │  │
│   │             to_effects               │  │
│   └──────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
```

The wasm side never makes network calls; the JS side does. That keeps
the wasm module deterministic and matches a wallet's preferred trust
model: signatures can be verified locally without ever leaving the page.
