"""Rockbox RL sample: 5x5 gridworld (Gymnasium-compatible).

Contract expected by the sandbox RL shim (see
core/crates/engine/src/modes/rl.rs::python_shim — PROTOCOL V2):

    reset([seed=...])   -> observation | (observation, info)
    step(action_bytes)  -> (obs, reward, terminated, truncated, info)
                           legacy (obs, reward, done[, info]) also accepted
    save()              -> picklable state
    restore(state)      -> None

Extras:
- `reset(seed=...)` is honoured automatically when the signature accepts it —
  deterministic rollouts for eval and parallel-seed sweeps. Pass the seed via
  episode settings: {"determinism": {"seed": 42}}.
- Include a "_obs_meta" dict in info (e.g. dtype/shape/encoding) and it is
  surfaced as tick["obs_meta"] so clients can decode raw obs bytes.
"""

import random

GRID = 5
GOAL = (GRID - 1, GRID - 1)

# Actions: 0=up, 1=down, 2=left, 3=right. Encoded as a single byte.
MOVES = {0: (-1, 0), 1: (1, 0), 2: (0, -1), 3: (0, 1)}
MAX_STEPS = 100

_state = {"pos": (0, 0), "steps": 0}
_rng = random.Random()


def _obs():
    r, c = _state["pos"]
    # 25-byte flat grid, 1 at agent, 2 at goal.
    buf = bytearray(GRID * GRID)
    buf[GOAL[0] * GRID + GOAL[1]] = 2
    buf[r * GRID + c] = 1
    return bytes(buf)


def _obs_meta():
    return {
        "dtype": "uint8",
        "shape": f"({GRID * GRID},)",
        "encoding": "raw",
        "legend": "0=empty,1=agent,2=goal",
    }


def reset(seed=None):
    if seed is not None:
        _rng.seed(seed)
    _state["pos"] = (0, 0)
    _state["steps"] = 0
    return _obs(), {"_obs_meta": _obs_meta()}


def observe():
    # Current observation without advancing state — used by checkpoint-resume
    # boots so a restored episode can report where it left off.
    return _obs()


def step(action):
    a = action[0] if isinstance(action, (bytes, bytearray)) and len(action) else 3
    dr, dc = MOVES.get(a, (0, 0))
    r, c = _state["pos"]
    nr, nc = max(0, min(GRID - 1, r + dr)), max(0, min(GRID - 1, c + dc))
    _state["pos"] = (nr, nc)
    _state["steps"] += 1

    at_goal = (nr, nc) == GOAL
    # Shaped reward: +1 at goal, small negative per step to encourage speed,
    # tiny positive for moving closer to goal.
    prev_dist = abs(r - GOAL[0]) + abs(c - GOAL[1])
    new_dist = abs(nr - GOAL[0]) + abs(nc - GOAL[1])
    shaping = 0.1 * (prev_dist - new_dist)
    reward = 1.0 if at_goal else (-0.01 + shaping)

    terminated = at_goal                      # true environment end state
    truncated = not at_goal and _state["steps"] >= MAX_STEPS  # time limit stop
    info = {"_obs_meta": _obs_meta(), "steps": _state["steps"], "pos": f"{nr},{nc}"}
    return _obs(), float(reward), bool(terminated), bool(truncated), info


def save():
    return dict(_state)


def restore(saved):
    _state.update(saved)
