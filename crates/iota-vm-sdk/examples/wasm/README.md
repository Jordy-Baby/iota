# Browser example

A minimal page that loads `iota-vm-sdk` compiled to wasm, runs an example
transaction through the local Move VM, and shows the result. It reuses the
crate's committed test fixtures to demonstrate both a valid and an invalid
outcome.

## Run

1. Build the wasm bundle (needs
   [`wasm-pack`](https://rustwasm.github.io/wasm-pack/installer/)):

   ```sh
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
