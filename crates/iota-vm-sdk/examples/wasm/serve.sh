#!/usr/bin/env bash
# Copyright (c) 2026 IOTA Stiftung
# SPDX-License-Identifier: Apache-2.0
#
# Build the wasm bundle if needed, then serve the crate root so the browser
# example can fetch its fixtures, and print the URL to open.
#
# Requires the wasm32 target (`rustup target add wasm32-unknown-unknown`) and
# the wasm-bindgen CLI matching the crate's wasm-bindgen version
# (`cargo install wasm-bindgen-cli --version <ver>`).
#
# Usage: ./serve.sh [port]   (default port 8000)
#        ./serve.sh --rebuild [port]   force a rebuild first
set -euo pipefail

example_dir="$(cd "$(dirname "$0")" && pwd)"
crate_dir="$(cd "$example_dir/../.." && pwd)"

rebuild=false
if [[ "${1:-}" == "--rebuild" ]]; then
  rebuild=true
  shift
fi
port="${1:-8000}"
url="http://localhost:$port/examples/wasm/"

# Build the bundle when it's missing, or when --rebuild is passed.
if [[ "$rebuild" == true || ! -f "$example_dir/pkg/iota_vm_sdk.js" ]]; then
  echo "Building wasm bundle…"
  cd "$crate_dir"
  out_dir="examples/wasm/pkg"
  target_dir="$(cargo metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
  wasm="$target_dir/wasm32-unknown-unknown/release/iota_vm_sdk.wasm"
  cargo build --lib --release --target wasm32-unknown-unknown --features wasm-bindgen
  wasm-bindgen "$wasm" --out-dir "$out_dir" --target web
fi

cd "$crate_dir"
echo
echo "Serving on $url"
echo "Press Ctrl+C to stop."
echo
python3 -m http.server "$port"
