"""Rockbox RLHF-style sample: reward-model scoring with KL regularisation.

A contextual bandit where a frozen "reward model" (deterministic, seeded
score table) rates each candidate response, and the reward applies an explicit
KL penalty against a reference policy — the standard RLHF reward shape:

    r = rm_score - beta * KL(pi || pi_ref)

The env tracks the running policy as exponential moving average of arm picks;
KL is computed between the last action distribution and the uniform reference.
info carries `rm_score`, `kl_penalty`, and a synthetic pairwise preference label
(`preferred_arm`) so preference-learning loops can consume the same ticks.

    reset([seed=...])   -> (context_bytes, info)
    step(action_byte)   -> (next_context, reward, terminated=False, truncated, info)

Actions: single byte 0/1 (arm pick).
"""

import math
import random

BETA = 0.1
MAX_STEPS = 20

_state = {"seed": 0, "rng": None, "step": 0, "ema": 0.5}


def _rm_score(context: int, arm: int) -> float:
    # Frozen "reward model": deterministic per (context, arm) score in [0, 1].
    h = (context * 2654435761 + arm * 40503) & 0xFFFF
    return (h % 1000) / 1000.0


def reset(seed=None):
    _state["seed"] = seed if seed is not None else 0
    _state["rng"] = random.Random(_state["seed"])
    _state["step"] = 0
    _state["ema"] = 0.5
    ctx = _state["rng"].randrange(8)
    info = {"_obs_meta": {"dtype": "uint8", "shape": "(1,)", "encoding": "raw", "kind": "bandit_context"},
            "beta": str(BETA)}
    return bytes([ctx]), info


def step(action):
    arm = action[0] if isinstance(action, (bytes, bytearray)) and len(action) else 0
    arm = 1 if arm else 0
    _state["step"] += 1
    ctx = (_state["rng"].randrange(8))
    rm = _rm_score(ctx, arm)

    # EMA policy vs uniform reference: |p - 0.5| is the per-step proxy for
    # total-variation distance; scaled to feel like a KL term.
    _state["ema"] = 0.9 * _state["ema"] + 0.1 * float(arm)
    kl = abs(_state["ema"] - 0.5)
    kl_scaled = BETA * kl * 2.0

    reward = rm - kl_scaled
    truncated = _state["step"] >= MAX_STEPS
    preferred_arm = 0 if _rm_score(ctx, 0) >= _rm_score(ctx, 1) else 1
    info = {
        "_obs_meta": {"dtype": "uint8", "shape": "(1,)", "encoding": "raw", "kind": "bandit_context"},
        "rm_score": f"{rm:.4f}",
        "kl_penalty": f"{kl_scaled:.4f}",
        "policy_ema": f"{_state['ema']:.3f}",
        "preference": f"arm{preferred_arm}>arm{1 - preferred_arm}",
        "step": str(_state["step"]),
    }
    return bytes([ctx]), float(reward), False, bool(truncated), info


def save():
    return {"seed": _state["seed"], "step": _state["step"], "ema": _state["ema"]}


def restore(saved):
    _state.update(saved)
    _state["rng"] = random.Random(_state["seed"])
