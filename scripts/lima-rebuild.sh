#!/usr/bin/env bash
# scripts/lima-rebuild.sh — sync source into the Lima VM and rebuild/restart
# the compose services. Use after changing Rust or Elixir code to pick the
# changes up (the stack itself does not hot-reload from the host; source is
# synced, not bind-mounted).
#
# Usage:
#   scripts/lima-rebuild.sh            # rebuild + restart everything
#   scripts/lima-rebuild.sh --no-sync  # skip rsync (rebuild only)

set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lima-common.sh

ensure_vm

if [[ ${1:-} != "--no-sync" ]]; then
  echo "→ syncing source into $LIMA_VM:~/rockbox" >&2
  sync_source
fi

vm_exec "cd ~/rockbox && ROCKBOX_API_PORT=$API_PORT ROCKBOX_PG_PORT=$PG_PORT docker compose up -d --build" >&2

echo "→ waiting for API health" >&2
for i in $(seq 1 120); do
  if vm_exec "curl -sf http://localhost:$API_PORT/health >/dev/null 2>&1"; then
    echo "stack healthy: http://localhost:$API_PORT" >&2
    exit 0
  fi
  sleep 5
done

die "API did not become healthy after 10 minutes. Check logs: just lima-logs"
