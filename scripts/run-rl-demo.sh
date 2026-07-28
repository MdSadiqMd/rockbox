#!/usr/bin/env bash
# scripts/run-rl-demo.sh - Demo RL gridworld: start episode, run N steps, destroy
#
# Usage:
#   scripts/run-rl-demo.sh                        # default 10 steps
#   scripts/run-rl-demo.sh 50                     # 50 steps
#   ROCKBOX_URL=... ROCKBOX_TOKEN=... scripts/run-rl-demo.sh

set -euo pipefail

URL="${ROCKBOX_URL:-http://localhost:4000}"
TOKEN="${ROCKBOX_TOKEN:-token-ws_pro_demo-pro}"
STEPS="${1:-10}"
ENV_FILE="priv/samples/rl/gridworld.py"

if [[ ! -f "$ENV_FILE" ]]; then
  echo "ERROR: env file $ENV_FILE not found" >&2
  exit 1
fi

content=$(jq -Rs . <"$ENV_FILE")

echo "=== RL Demo: gridworld ($STEPS steps) ===" >&2

# 1. Start episode
echo "→ POST $URL/api/rl/episodes (start)" >&2
start_resp=$(curl -sS -X POST "$URL/api/rl/episodes" \
  -H "authorization: Bearer $TOKEN" \
  -H "content-type: application/json" \
  -d "$(jq -n \
    --argjson content "$content" \
    '{settings: {
       language: "python",
       runtime: "python-base",
       entrypoint: "gridworld.py",
       files: [{path: "gridworld.py", content: $content}],
       limits: {wall_ms: 10000}
     }}')")

episode_id=$(echo "$start_resp" | jq -r '.episode_id // empty')
vm_id=$(echo "$start_resp" | jq -r '.vm_id // empty')

if [[ -z "$episode_id" || -z "$vm_id" ]]; then
  echo "ERROR: failed to start episode" >&2
  echo "$start_resp" | jq >&2
  exit 1
fi

echo "  episode_id=$episode_id  vm_id=$vm_id" >&2
echo "  initial obs: $(echo "$start_resp" | jq -c '.initial')" >&2

# 2. Run steps
total_reward=0
for i in $(seq 1 "$STEPS"); do
  # Random action 0..3 (up/down/left/right), encoded as single byte then base64
  action=$((RANDOM % 4))
  action_b64=$(printf "\\x$(printf '%02x' "$action")" | base64)

  step_resp=$(curl -sS -X POST "$URL/api/rl/episodes/$episode_id/step?vm_id=$vm_id" \
    -H "authorization: Bearer $TOKEN" \
    -H "content-type: application/json" \
    -d "$(jq -n --arg a "$action_b64" '{action: $a}')")

  reward=$(echo "$step_resp" | jq -r '.reward // 0')
  done=$(echo "$step_resp" | jq -r '.done // false')
  info=$(echo "$step_resp" | jq -c '.info // {}')
  total_reward=$(echo "$total_reward + $reward" | bc -l)

  printf "  step %3d: action=%d reward=%+.3f done=%s info=%s\n" "$i" "$action" "$reward" "$done" "$info" >&2

  if [[ "$done" == "true" ]]; then
    echo "  → episode done at step $i" >&2
    break
  fi
done

printf "  total_reward=%.3f\n" "$total_reward" >&2

# 3. Destroy episode
echo "→ DELETE $URL/api/rl/episodes/$episode_id (destroy)" >&2
curl -sS -X DELETE "$URL/api/rl/episodes/$episode_id?vm_id=$vm_id" \
  -H "authorization: Bearer $TOKEN" | jq >&2

echo "=== Done ===" >&2

# Output summary as JSON for scripting
jq -n \
  --arg episode_id "$episode_id" \
  --arg vm_id "$vm_id" \
  --argjson steps "$i" \
  --argjson total_reward "$total_reward" \
  '{episode_id: $episode_id, vm_id: $vm_id, steps: $steps, total_reward: $total_reward}'
