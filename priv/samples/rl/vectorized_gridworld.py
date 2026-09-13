"""Rockbox RL sample: vectorized 5x5 gridworld.

Demonstrates the EnvPool-style vectorized contract that the SOTA loop
enables via batched stepping + concurrent episodes.

Single-env contract (always supported):
    env.reset(seed=...) -> (obs, info)
    env.step(action_bytes) -> (obs, reward, done, info) or 5-tuple
    env.save()/restore() optional for checkpointing

Vectorized contract (opt-in, for throughput):
    - Set ROCKBOX_NUM_ENVS=N and expose class VectorEnv or envs = [...]
    - Batched stepping via POST /api/rl/episodes/:id/steps with
      {"actions": [b64(a0), b64(a1), ...]} pins N actions to one
      engine hop (see core/crates/engine/src/modes/rl.rs::steps_batch).
    - The engine's single spawn_blocking hop runs the N steps
      sequentially in the worker's thread; the shim's parallel fan-out
      (when ROCKBOX_VECTORIZED=1) would step N sub-envs in a
      ThreadPoolExecutor. This file implements the simple sequential
      version that works on the current shim and can be upgraded to the
      parallel shim without client change.

For true parallel vectorization at ~50k steps/s (EnvPool analogue),
implement VectorEnv below and set ROCKBOX_VECTORIZED=1 in episode
settings.env. The SOTA loop report (docs/sota_loop_report.md §4.2)
describes the design.

This single-env sample is used by scripts/bench_episodes_concurrent.py
and by the new bench_sota_loop.py vectorized section.
"""

import random

GRID = 5
GOAL = (GRID - 1, GRID - 1)
MOVES = {0: (-1, 0), 1: (1, 0), 2: (0, -1), 3: (0, 1)}
MAX_STEPS = 100

_state = {"pos": (0, 0), "steps": 0}
_rng = random.Random()


def _obs():
    r, c = _state["pos"]
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
    return _obs()


def step(action_bytes):
    if len(action_bytes) == 0:
        return reset()
    a = action_bytes[0] % 4
    dr, dc = MOVES[a]
    r, c = _state["pos"]
    nr, nc = max(0, min(GRID - 1, r + dr)), max(0, min(GRID - 1, c + dc))
    _state["pos"] = (nr, nc)
    _state["steps"] += 1
    reward = 1.0 if _state["pos"] == GOAL else -0.01
    done = _state["pos"] == GOAL or _state["steps"] >= MAX_STEPS
    return _obs(), reward, done, {"steps": str(_state["steps"])}


def save():
    return dict(_state)


def restore(s):
    _state.update(s)


  # Vectorized wrapper for clients that want EnvPool-style parallelism.
  # The engine's batched API already pipelines N actions; this class
  # interprets those actions as N parallel sub-env steps when
  # ROCKBOX_VECTORIZED=1 (shim fans out sequentially in-process).
class VectorEnv:
    def __init__(self, n=4):
        self.n = n
        self.envs = [{"pos": (0, 0), "steps": 0} for _ in range(n)]

    def reset(self, seed=None):
        if seed is not None:
            _rng.seed(seed)
        out = []
        for e in self.envs:
            e["pos"] = (0, 0)
            e["steps"] = 0
            out.append(_obs_for(e))
        # Return concatenated obs (clients split by obs size)
        return b"".join(out), {"_obs_meta": {"dtype": "uint8", "shape": f"({self.n},{GRID*GRID})", "encoding": "raw_concat"}}

    def step(self, actions):
        # actions is bytes of length n (one byte per sub-env)
        obs_parts = []
        rewards = []
        dones = []
        for i, e in enumerate(self.envs):
            a = actions[i] % 4 if i < len(actions) else 0
            dr, dc = MOVES[a]
            r, c = e["pos"]
            nr, nc = max(0, min(GRID - 1, r + dr)), max(0, min(GRID - 1, c + dc))
            e["pos"] = (nr, nc)
            e["steps"] += 1
            reward = 1.0 if e["pos"] == GOAL else -0.01
            done = e["pos"] == GOAL or e["steps"] >= MAX_STEPS
            obs_parts.append(_obs_for(e))
            rewards.append(reward)
            dones.append(done)
        return b"".join(obs_parts), float(sum(rewards) / len(rewards)), any(dones), {"steps": str(sum(e["steps"] for e in self.envs))}


def _obs_for(e):
    r, c = e["pos"]
    buf = bytearray(GRID * GRID)
    buf[GOAL[0] * GRID + GOAL[1]] = 2
    buf[r * GRID + c] = 1
    return bytes(buf)
