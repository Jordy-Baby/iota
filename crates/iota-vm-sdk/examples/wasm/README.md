# Browser example

A minimal page that loads `iota-vm-sdk` compiled to wasm, runs an example
transaction through the local Move VM, and shows the result (status, gas, the
created/mutated/deleted objects, and decoded events). It reuses the crate's
committed test fixtures to demonstrate three outcomes: a valid authenticator, a
rejected signature, and a staking transaction that emits events.

## Run

This needs the `wasm32-unknown-unknown` target and a `wasm-bindgen` CLI matching
the crate's `wasm-bindgen` version (run `cargo tree -p iota-vm-sdk -i
wasm-bindgen` to check it):

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version <ver>   # e.g. 0.2.122
```

Then one command builds the bundle (if missing) and serves it, printing the URL
to open:

```sh
./serve.sh            # optional: ./serve.sh <port> (default 8000)
```

Open the printed <http://localhost:8000/examples/wasm/> and pick an example.

`serve.sh` only builds when the bundle is missing; pass `--rebuild` to force a
rebuild after changing the crate (`./serve.sh --rebuild`). It serves the crate
root because the page fetches fixtures from `../../tests/fixtures`.

## Regenerating the staking fixture

`tests/fixtures/stake.json` is captured from the end-to-end comparison test,
which builds a staking transaction against a local `TestCluster` and pre-fetches
the objects it touches. To refresh it:

```sh
cd ../..            # crates/iota-vm-sdk
IOTA_VM_SDK_DUMP_FIXTURE="$PWD/tests/fixtures/stake.json" \
  cargo test --features grpc --test e2e_comparison -- --nocapture
```
