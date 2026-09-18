#!/usr/bin/env bash
# scripts/lima-down.sh — stop the compose stack (and optionally the VM).
#
# Usage:
#   scripts/lima-down.sh            # stop stack, keep VM running
#   scripts/lima-down.sh --vm       # also stop the Lima VM

set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lima-common.sh

ensure_vm_running

if vm_exec 'cd ~/rockbox 2>/dev/null && docker compose down'; then :; else
  echo "stack was not running" >&2
fi

if [[ ${1:-} == "--vm" ]]; then
  echo "→ stopping VM '$LIMA_VM'" >&2
  limactl stop "$LIMA_VM"
fi
