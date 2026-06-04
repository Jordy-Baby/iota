#!/usr/bin/env bash
# Copyright (c) 2026 IOTA Stiftung
# SPDX-License-Identifier: Apache-2.0
#
# Build the wasm bundle for the browser example into examples/wasm/pkg.
#
# Requires the wasm32 target (`rustup target add wasm32-unknown-unknown`) and
# the wasm-bindgen CLI matching the crate's wasm-bindgen version
# (`cargo install wasm-bindgen-cli --version <ver>`).
set -euo pipefail

crate_dir="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$crate_dir"

out_dir="examples/wasm/pkg"
target_dir="$(cargo metadata --format-version 1 --no-deps \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
wasm="$target_dir/wasm32-unknown-unknown/release/iota_vm_sdk.wasm"

cargo build --lib --release --target wasm32-unknown-unknown --features wasm-bindgen
wasm-bindgen "$wasm" --out-dir "$out_dir" --target web
