"""Rockbox sample: CartPole-lite (classic control, pure Python).

Standard CartPole-v1 physics with float32-packed state observations — no
external dependencies, deterministic given the seed. Exercises multi-byte raw
observations and time-limit truncation.

    reset([seed=...])   -> (state_f32x4, info)
    step(action_byte)   -> (next_state, reward, terminated, truncated, info)

Actions: 0 = push left, 1 = push right. reward +1 per surviving step.
Terminated on pole fall / cart out of bounds; truncated at 200 steps.
"""

import math
import random
import struct

G = 9.8
MASS_CART = 1.0
MASS_POLE = 0.1
TOTAL_MASS = MASS_CART + MASS_POLE
LENGTH = 0.5
POLEMASS_LENGTH = MASS_POLE * LENGTH
FORCE_MAG = 10.0
TAU = 0.02
THETA_THRESHOLD = 0.2095  # 12 degrees
X_THRESHOLD = 2.4
STEPS_LIMIT = 200
F32_MAX = 3.4028235e38

_state = {"x": 0.0, "xd": 0.0, "th": 0.0, "thd": 0.0, "steps": 0}


def _dynamics(state, force):
    x, xd, th, thd = state
    cos = math.cos(th)
    sin = math.sin(th)
    temp = (force + POLEMASS_LENGTH * thd * thd * sin) / TOTAL_MASS
    thacc = (G * sin - cos * temp) / (LENGTH * (4.0 / 3.0 - MASS_POLE * cos * cos / TOTAL_MASS))
    xacc = temp - POLEMASS_LENGTH * thacc * cos / TOTAL_MASS
    # semi-implicit Euler keeps it stable enough for a toy env
    return (x + TAU * xd, xd + TAU * xacc, th + TAU * thd, thd + TAU * thacc)


def _obs():
    # Clamp before packing: an unstable trajectory can exceed f32 range and
    # struct.pack would raise instead of terminating the episode.
    def f32(v):
        if v != v:
            return 0.0
        return max(-F32_MAX, min(F32_MAX, v))

    return struct.pack("<ffff", f32(_state["x"]), f32(_state["xd"]),
                       f32(_state["th"]), f32(_state["thd"]))


def reset(seed=None):
    rng = random.Random(seed)
    _state.update(x=rng.uniform(-0.05, 0.05), xd=rng.uniform(-0.05, 0.05),
                  th=rng.uniform(-0.05, 0.05), thd=rng.uniform(-0.05, 0.05), steps=0)
    info = {"_obs_meta": {"dtype": "float32", "shape": "(4,)", "encoding": "raw-le",
                          "fields": "x,xdot,theta,thetadot"}}
    return _obs(), info


def step(action):
    a = action[0] if isinstance(action, (bytes, bytearray)) and len(action) else 0
    force = FORCE_MAG if a else -FORCE_MAG
    nxt = _dynamics((_state["x"], _state["xd"], _state["th"], _state["thd"]), force)
    _state.update(x=nxt[0], xd=nxt[1], th=nxt[2], thd=nxt[3])
    _state["steps"] += 1

    terminated = abs(_state["x"]) > X_THRESHOLD or abs(_state["th"]) > THETA_THRESHOLD
    truncated = not terminated and _state["steps"] >= STEPS_LIMIT
    info = {"steps": str(_state["steps"]),
            "_obs_meta": {"dtype": "float32", "shape": "(4,)", "encoding": "raw-le"}}
    return _obs(), 1.0, bool(terminated), bool(truncated), info


def save():
    return dict(_state)


def restore(saved):
    _state.update(saved)
