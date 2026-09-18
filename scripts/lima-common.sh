#!/usr/bin/env bash
# scripts/lima-common.sh — shared helpers for driving the Rockbox dev stack
# inside the project-owned Lima VM. Sourced by the lima-* scripts and Justfile
# recipes; not meant to be executed directly.
#
# The VM belongs to THIS repo: its definition lives at ./lima.yaml and its
# disks/state under ./.lima (LIMA_HOME is scoped to the project root). Other
# Lima instances on the machine — including ones from other projects — are
# never started, stopped, or reused. When the instance does not exist yet,
# `ensure_vm` creates it from lima.yaml and Lima downloads the cloud image
# on first start.
#
# Every entry point hard-fails with an actionable message when a
# prerequisite is missing (limactl not installed, stack not up) so users get
# a fix-it hint instead of a cryptic SSH error.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

export LIMA_HOME="${ROCKBOX_LIMA_HOME:-$REPO_ROOT/.lima}"
LIMA_VM="${ROCKBOX_LIMA_VM:-rockbox}"
LIMA_TEMPLATE="$REPO_ROOT/lima.yaml"

API_PORT="${ROCKBOX_API_PORT:-4000}"
PG_PORT="${ROCKBOX_PG_PORT:-5433}"

LIMA_SSH_CONFIG="$LIMA_HOME/$LIMA_VM/ssh.config"
LIMA_SSH="ssh -F ${LIMA_SSH_CONFIG} lima-${LIMA_VM}"
REMOTE_DIR='~/rockbox'

die() { echo "ERROR: $*" >&2; exit 1; }

require_limactl() {
  command -v limactl >/dev/null 2>&1 || die "limactl not found on PATH. Install it: brew install lima"
}

vm_exists() {
  require_limactl
  limactl list 2>/dev/null | awk '{print $1}' | grep -qx "$LIMA_VM"
}

vm_running() {
  limactl list 2>/dev/null | awk -v vm="$LIMA_VM" '$1==vm && $2=="Running"' | grep -q .
}

# Create the instance from the repo-root template if missing (image is
# downloaded on demand), then make sure it is running.
ensure_vm() {
  require_limactl

  if ! vm_exists; then
    echo "→ creating Lima VM '$LIMA_VM' from lima.yaml (state: $LIMA_HOME)…" >&2
    echo "  first boot downloads the Ubuntu cloud image — this takes a few minutes." >&2

    local args=(create --name "$LIMA_VM")
    [[ -n ${ROCKBOX_LIMA_CPUS:-} ]] && args+=(--cpus "$ROCKBOX_LIMA_CPUS")
    # --memory takes a bare number in GiB
    [[ -n ${ROCKBOX_LIMA_MEMORY:-} ]] && args+=(--memory "${ROCKBOX_LIMA_MEMORY%%[*G]*}")

    limactl "${args[@]}" "$LIMA_TEMPLATE" >&2 || die "creating VM '$LIMA_VM' failed"
  fi

  ensure_vm_running
}

ensure_vm_running() {
  require_limactl
  vm_exists || die "Lima VM '$LIMA_VM' does not exist. Run: just lima-up (creates it)"
  if ! vm_running; then
    echo "→ starting Lima VM '$LIMA_VM'…" >&2
    limactl start "$LIMA_VM" >&2
  fi
}

require_ssh_config() {
  [[ -f $LIMA_SSH_CONFIG ]] || die "ssh config missing at $LIMA_SSH_CONFIG (is VM '$LIMA_VM' created?)"
}

# Docker provisioning happens during first boot; a freshly created VM
# sometimes needs one restart before the daemon + group membership are live.
require_docker() {
  if ! vm_exec 'docker info >/dev/null 2>&1'; then
    echo "→ docker not ready yet; restarting VM once (first-boot provisioning)…" >&2
    limactl stop "$LIMA_VM" >&2 && limactl start "$LIMA_VM" >&2
    sleep 10
    vm_exec 'docker info >/dev/null 2>&1' ||
      die "docker is not usable inside VM '$LIMA_VM'. Check: limactl shell $LIMA_VM -- docker info"
  fi
}

# Run a command inside the VM.
vm_exec() { $LIMA_SSH "$@"; }

# True when the rockbox compose stack reports healthy in the VM.
stack_healthy() {
  vm_exec "cd ~/rockbox 2>/dev/null && docker compose ps --format \"{{.Service}} {{.Status}}\" 2>/dev/null" 2>/dev/null |
    grep -E '^app .*(healthy|running)' >/dev/null
}

require_stack() {
  ensure_vm_running
  require_ssh_config
  stack_healthy || die "Rockbox stack is not running in VM '$LIMA_VM'. Start it: just lima-up"
}

# Sync the source tree from the host into the VM (build artifacts excluded —
# they live in named volumes / container-internal caches).
sync_source() {
  ensure_vm_running
  require_ssh_config
  vm_exec "mkdir -p ~/rockbox"
  rsync -az --delete \
    -e "ssh -F $LIMA_SSH_CONFIG" \
    --exclude .git --exclude deps --exclude _build --exclude core/target \
    --exclude .elixir_ls --exclude .phx.log --exclude '*.beam' \
    --exclude .lima \
    ./ "lima-${LIMA_VM}:rockbox/"
}
