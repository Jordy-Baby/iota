# Browser example

A minimal page that loads `iota-vm-sdk` compiled to wasm, runs an example
transaction through the local Move VM, and shows the result (status, gas, the
created/mutated/deleted objects, and decoded events). It reuses the crate's
committed test fixtures to demonstrate three outcomes: a valid authenticator, a
rejected signature, and a staking transaction that emits events.

## Run

1. Build the wasm bundle. This needs the `wasm32-unknown-unknown` target and a
   `wasm-bindgen` CLI matching the crate's `wasm-bindgen` version (run
   `cargo tree -p iota-vm-sdk -i wasm-bindgen` to check it):

      ```sh
   rustup target add wasm32-unknown-unknown
   cargo install wasm-bindgen-cli --version <ver>   # e.g. 0.2.122
   ./build.sh
   ```

2. Serve the crate root and open the example (the page fetches fixtures from
   `../../tests/fixtures`, so it must be served from the crate root, not this
   directory):

   ```sh
      cd ../..            # crates/iota-vm-sdk
      python3 -m http.server 8000
   ```

   Then open <http://localhost:8000/examples/wasm/> and pick an example.

## Regenerating the staking fixture

`tests/fixtures/stake.json` is captured from the end-to-end comparison test,
which builds a staking transaction against a local `TestCluster` and pre-fetches
the objects it touches. To refresh it:

```sh
cd ../..            # crates/iota-vm-sdk
IOTA_VM_SDK_DUMP_FIXTURE="$PWD/tests/fixtures/stake.json" \
  cargo test --features grpc --test e2e_comparison -- --nocapture
```
