#!/usr/bin/env bash
# scripts/bench-engine.sh — engine sandbox latency/throughput benchmark
#
# Two modes:
#   warm — one long-lived engine process fed N frames back-to-back
#          (measures steady-state per-request cost incl. sandbox launch)
#   cold — N fresh engine processes, one frame each
#          (adds process spawn + tokio boot, the orchestrator-per-request path)
#
# Usage:
#   scripts/bench-engine.sh <frame.bin> [iterations] [warm|cold]
#   FRAME=... ITER=50 MODE=cold scripts/bench-engine.sh

set -euo pipefail

FRAME="${FRAME:-${1:-/tmp/rb-frame.bin}}"
ITER="${ITER:-${2:-50}}"
MODE="${MODE:-${3:-warm}}"
ENGINE="${ENGINE:-/app/core/target/debug/engine}"

[[ -f "$FRAME" ]] || { echo "frame not found: $FRAME" >&2; exit 64; }
[[ -x "$ENGINE" ]] || { echo "engine not found: $ENGINE" >&2; exit 64; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if [[ "$MODE" == "warm" ]]; then
  python3 - "$FRAME" "$ITER" "$WORK/in.bin" <<'PY'
import sys
frame = open(sys.argv[1], "rb").read()
n = int(sys.argv[2])
with open(sys.argv[3], "wb") as f:
    for _ in range(n):
        f.write(frame)
PY
  start=$(date +%s%N)
  # stdout must stay a pipe: the engine epoll-registers fd 1, and a regular
  # file target fails EPOLL_CTL_ADD with EPERM.
  cat "$WORK/in.bin" | timeout 300 "$ENGINE" --log error 2> "$WORK/err.log" | cat > "$WORK/out.bin" || true
  end=$(date +%s%N)
else
  start=$(date +%s%N)
  for _ in $(seq "$ITER"); do
    cat "$FRAME" | timeout 60 "$ENGINE" --log error 2>> "$WORK/err.log" | cat >> "$WORK/out.bin" || true
  done
  end=$(date +%s%N)
fi

python3 - "$WORK/out.bin" "$ITER" "$MODE" $((end - start)) <<'PY'
import json
import sys
import statistics

path, n, mode, wall_ns = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
data = open(path, "rb").read()

def mp_int(data, pos):
    b = data[pos]
    if 0x00 <= b <= 0x7F:
        return b, pos + 1
    if 0xE0 <= b <= 0xFF:
        return b - 0x100, pos + 1
    if b == 0xCC:
        return data[pos + 1], pos + 2
    if b == 0xCD:
        return int.from_bytes(data[pos + 1:pos + 3], "big"), pos + 3
    if b == 0xCE:
        return int.from_bytes(data[pos + 1:pos + 5], "big"), pos + 5
    if b == 0xD0:
        return int.from_bytes(data[pos + 1:pos + 2], "big", signed=True), pos + 2
    if b == 0xD1:
        return int.from_bytes(data[pos + 1:pos + 3], "big", signed=True), pos + 3
    if b == 0xD2:
        return int.from_bytes(data[pos + 1:pos + 5], "big", signed=True), pos + 5
    raise ValueError(f"not an int: 0x{b:02x} at {pos}")

def mp_str(data, pos):
    b = data[pos]
    if 0xA0 <= b <= 0xBF:
        ln, pos = b - 0xA0, pos + 1
    elif b == 0xD9:
        ln, pos = data[pos + 1], pos + 2
    elif b == 0xDA:
        ln, pos = int.from_bytes(data[pos + 1:pos + 3], "big"), pos + 3
    elif b == 0xDB:
        ln, pos = int.from_bytes(data[pos + 1:pos + 5], "big"), pos + 5
    else:
        raise ValueError(f"not a str: 0x{b:02x} at {pos}")
    return data[pos:pos + ln].decode("utf-8", "replace"), pos + ln

def mp_skip(data, pos):
    b = data[pos]
    if 0x00 <= b <= 0x7F or 0xE0 <= b <= 0xFF:
        return pos + 1
    if 0x80 <= b <= 0x8F:  # map
        n = b - 0x80
        pos += 1
        for _ in range(2 * n):
            pos = mp_skip(data, pos)
        return pos
    if 0x90 <= b <= 0x9F:  # array
        n = b - 0x90
        pos += 1
        for _ in range(n):
            pos = mp_skip(data, pos)
        return pos
    if 0xA0 <= b <= 0xBF:
        return pos + 1 + (b - 0xA0)
    if 0xC0 <= b <= 0xC7:
        return pos + (1 if b == 0xC0 else 2)
    if b == 0xC8:
        return pos + 3
    if b == 0xC9:
        return pos + 5
    if 0xCA <= b <= 0xCB:
        return pos + (5 if b == 0xCA else 9)
    if 0xCC <= b <= 0xCF:
        return pos + (2 if b == 0xCC else 3 if b == 0xCD else 5 if b == 0xCE else 9)
    if 0xD0 <= b <= 0xD3:
        return pos + (2 if b == 0xD0 else 3 if b == 0xD1 else 5 if b == 0xD2 else 9)
    if 0xD4 <= b <= 0xD8:
        return pos + 2
    if 0xD9:
        return pos + 2 + data[pos + 1]
    if 0xDA:
        return pos + 3 + int.from_bytes(data[pos + 1:pos + 3], "big")
    if 0xDB:
        return pos + 5 + int.from_bytes(data[pos + 1:pos + 5], "big")
    if 0xDC:
        return pos + 3 + 2 * int.from_bytes(data[pos + 1:pos + 3], "big")
    if 0xDD:
        return pos + 5 + 2 * int.from_bytes(data[pos + 1:pos + 5], "big")
    if 0xDE:
        return pos + 3 + 4 * int.from_bytes(data[pos + 1:pos + 3], "big")
    if 0xDF:
        return pos + 5 + 4 * int.from_bytes(data[pos + 1:pos + 5], "big")
    raise ValueError(f"cannot skip 0x{b:02x} at {pos}")

def mp_map_get(data, pos, key):
    b = data[pos]
    if not (0x80 <= b <= 0x8F):
        return None
    n = b - 0x80
    pos += 1
    for _ in range(n):
        k, pos = mp_str(data, pos)
        vpos = pos
        if k == key:
            return vpos
        pos = mp_skip(data, pos)
    return None

def mp_str_at(data, pos):
    s, _ = mp_str(data, pos)
    return s

def mp_int_at(data, pos):
    i, _ = mp_int(data, pos)
    return i

results = []
pos = 0
while pos + 4 <= len(data):
    ln = int.from_bytes(data[pos:pos + 4], "big")
    pos += 4
    if pos + ln > len(data):
        break
    frame = data[pos:pos + ln]
    pos += ln
    try:
        vpos = mp_map_get(frame, 0, "type")
        if vpos is not None and mp_str_at(frame, vpos) == "result":
            results.append(frame)
    except (ValueError, IndexError, UnicodeDecodeError):
        continue

statuses = {}
exec_ms = []
for f in results:
    sp = mp_map_get(f, 0, "status")
    st = mp_str_at(f, sp) if sp is not None else "?"
    statuses[st] = statuses.get(st, 0) + 1
    ep = mp_map_get(f, 0, "exec_time_ms")
    if ep is not None:
        exec_ms.append(mp_int_at(f, ep))

exec_ms.sort()
per_req_ms = wall_ns / max(n, 1) / 1e6

print(f"mode            {mode}")
print(f"requests        {n}")
print(f"result frames   {len(results)}")
print(f"status          {statuses}")
print(f"wall total      {wall_ns / 1e6:.1f} ms")
print(f"wall/request    {per_req_ms:.2f} ms   ({1000 / max(per_req_ms, 1e-9):.1f} req/s)")
if exec_ms:
    print(f"exec_time_ms    min={exec_ms[0]} p50={statistics.median(exec_ms)} avg={statistics.mean(exec_ms):.1f} p95={exec_ms[int(len(exec_ms) * 0.95) - 1]} max={exec_ms[-1]}")
PY
