#!/usr/bin/env bash
set -euo pipefail
# Build the Rust execution engine + compiler in release mode
# Outputs:
#   core/target/release/engine
#   core/target/release/compiler

cd "$(dirname "$0")/.."
cargo build --manifest-path core/Cargo.toml --release --workspace --bins
echo
echo "Built:"
ls -la core/target/release/engine core/target/release/compiler
