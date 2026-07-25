#!/usr/bin/env bash
set -euo pipefail
# Pre-evaluate each catalogued flake and cache its devShell env on the host
# Idempotent, `nix eval` is reproducible

cd "$(dirname "$0")/.."

CACHE_DIR="${ROCKBOX_FLAKE_CACHE:-/etc/sandbox/flakes}"
sudo mkdir -p "$CACHE_DIR"

for flake_dir in nix/flakes/*/; do
  name=$(basename "$flake_dir")
  echo ">> evaluating $name"
  nix flake update --flake "$flake_dir"
  sudo cp "$flake_dir/flake.lock" "$CACHE_DIR/$name.lock"
done

echo "Pre-warmed catalog at $CACHE_DIR:"
ls -la "$CACHE_DIR"
