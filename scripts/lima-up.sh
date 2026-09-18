#!/usr/bin/env bash
# scripts/lima-up.sh — bring the Rockbox dev stack up inside the Lima VM.
#
# This is the supported path for macOS hosts: Linux sandboxing primitives
# (user NS, seccomp, cgroups) only work on Linux, and Lima provides that VM.
# The VM is project-owned (lima.yaml + ./.lima state dir): if it does not
# exist yet it is created here and the Ubuntu image is downloaded on first
# boot. Other Lima instances on the machine are never touched.
#
# Usage:
#   scripts/lima-up.sh              # create-if-missing, sync source, build, start
#   ROCKBOX_API_PORT=4001 scripts/lima-up.sh   # when another VM already owns host :4000

set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lima-common.sh

ensure_vm

echo "→ syncing source into $LIMA_VM:~/rockbox" >&2
sync_source

require_docker

echo "→ building + starting compose stack in $LIMA_VM" >&2
vm_exec "cd ~/rockbox && ROCKBOX_API_PORT=$API_PORT ROCKBOX_PG_PORT=$PG_PORT docker compose up -d --build" >&2

echo "→ waiting for API health at http://localhost:$API_PORT/health" >&2
for i in $(seq 1 120); do
  if vm_exec "curl -sf http://localhost:$API_PORT/health >/dev/null 2>&1"; then
    echo "stack healthy: http://localhost:$API_PORT (Lima forwards to host; VM '$LIMA_VM')" >&2
    exit 0
  fi
  sleep 5
done

die "API did not become healthy after 10 minutes. Check logs: just lima-logs"
