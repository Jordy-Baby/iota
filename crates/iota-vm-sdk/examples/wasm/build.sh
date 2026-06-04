#!/usr/bin/env bash
# Copyright (c) 2026 IOTA Stiftung
# SPDX-License-Identifier: Apache-2.0
#
# Build the wasm bundle for the browser example into examples/wasm/pkg.
# Requires wasm-pack: https://rustwasm.github.io/wasm-pack/installer/
set -euo pipefail

crate_dir="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$crate_dir"

wasm-pack build --target web --features wasm-bindgen --out-dir examples/wasm/pkg
