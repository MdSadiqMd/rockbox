#!/usr/bin/env bash
# scripts/bench-api.sh — end-to-end HTTP benchmark: Phoenix + engine + sandbox
#
# Usage:
#   scripts/bench-api.sh [iterations] [concurrency]
#   URL=... TOKEN=... ITER=50 CONC=5 scripts/bench-api.sh
#
# Requires: jq, curl, and GNU time (or a date with %N). Serial by default;
# CONC>1 uses background jobs (adds scheduler noise on the Elixir side).

set -euo pipefail

URL="${URL:-http://localhost:4000}"
TOKEN="${TOKEN:-token-ws_pro_demo-pro}"
ITER="${ITER:-${1:-30}}"
CONC="${CONC:-${2:-1}}"
CODE="${CODE:-print(42)}"
PLANG="${PLANG:-python}"

payload() {
  jq -n \
    --arg lang "$PLANG" \
    --arg code "$CODE" \
    '{settings: {language: $lang, entrypoint: "main.'$PLANG'",
                 files: [{path: "main.'$PLANG'", content: $code}],
                 limits: {wall_ms: 5000}}}'
}

req() {
  curl -sS -w '\n%{time_total}\n%{http_code}\n' -X POST "$URL/api/execute" \
    -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    -d "$(payload)"
}

echo "bench-api  url=$URL iter=$ITER conc=$CONC lang=$PLANG" >&2
echo "warming up (2 requests)…" >&2
req > /dev/null || true
req > /dev/null || true

TIMES=()
FAILS=0
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
start=$(date +%s%N)
if [[ "$CONC" -eq 1 ]]; then
  for i in $(seq "$ITER"); do
    out=$(req) || { FAILS=$((FAILS + 1)); continue; }
    body=$(printf "%s" "$out" | sed '$d' | sed '$d')
    line=$(printf "%s" "$out" | tail -n 2 | head -1)
    code=$(printf "%s" "$out" | tail -n 1)
    [[ "$code" == "200" ]] || { FAILS=$((FAILS + 1)); continue; }
    TIMES+=("$line")
    if [[ ${VERBOSE:-0} == 1 ]]; then
      echo "$body" | jq -c '{status, exit_code, exec_time_ms, memory_peak_mb}'
    fi
  done
else
  for i in $(seq "$ITER"); do
    req > "$WORK/bench-$i.out" 2>/dev/null &
  done
  wait
  for f in "$WORK"/bench-*.out; do
    line=$(tail -n 2 "$f" | head -1)
    code=$(tail -n 1 "$f")
    [[ "$code" == "200" ]] || { FAILS=$((FAILS + 1)); rm -f "$f"; continue; }
    TIMES+=("$line")
    rm -f "$f"
  done
fi
end=$(date +%s%N)

python3 - "$ITER" "$FAILS" "$((end - start))" "${TIMES[@]}" <<'PY'
import statistics
import sys

n, fails, wall_ns = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
times = sorted(float(t) * 1000 for t in sys.argv[4:])
per = wall_ns / max(n, 1) / 1e6
print(f"requests        {n}  (failures: {fails})")
print(f"wall/request    {per:.2f} ms   ({1000 / max(per, 1e-9):.1f} req/s)")
if times:
    print(f"curl latency    min={times[0]:.1f} p50={statistics.median(times):.1f} avg={statistics.mean(times):.1f} p95={times[int(len(times) * 0.95) - 1]:.1f} max={times[-1]:.1f} ms")
PY
