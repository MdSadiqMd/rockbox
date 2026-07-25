#!/usr/bin/env bash
set -euo pipefail
# Build the /dev-min template directory
# Must be run as root because mknod requires CAP_MKNOD

target="${1:-/var/lib/sandbox/dev-min}"

if [[ $EUID -ne 0 ]]; then
  echo "must run as root" >&2
  exit 1
fi

mkdir -p "$target"
cd "$target"

# c $major $minor - character device
mknod -m 666 null    c 1 3
mknod -m 666 zero    c 1 5
mknod -m 666 full    c 1 7
mknod -m 444 urandom c 1 9
mknod -m 444 random  c 1 8
mknod -m 666 tty     c 5 0

chown -R root:root "$target"
chmod 555 "$target"

echo "Built /dev-min template at $target:"
ls -la "$target"
