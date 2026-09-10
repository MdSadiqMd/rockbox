"""Rockbox RLVR sample: math answer verification (verifiable rewards).

The observation is a generated arithmetic problem (utf-8 text); the agent
submits an answer as utf-8 text. Reward comes purely from an exact verifier —
no learned reward model. Gymnasium-compatible with the Rockbox shim contract:

    reset([seed=...])   -> (problem_text, info)
    step(answer_text)   -> (feedback_text, reward, terminated, truncated, info)

Reward: +1 exact match after normalisation, -0.01 otherwise. Episode ends on
the first correct answer (terminated) or after MAX_ATTEMPTS (truncated).
"""

import random

MAX_ATTEMPTS = 3

_state = {"problem": None, "answer": None, "attempts": 0}


def _gen_problem(rng):
    style = rng.randrange(3)
    if style == 0:
        a, b = rng.randint(11, 99), rng.randint(11, 99)
        return f"{a} + {b} = ?", str(a + b)
    if style == 1:
        a, b = rng.randint(12, 99), rng.randint(3, 9)
        return f"{a} * {b} = ?", str(a * b)
    a, b = rng.randint(20, 99), rng.randint(5, 19)
    return f"{max(a, b)} - {min(a, b)} = ?", str(abs(a - b))


def _normalise(text):
    digits = "".join(ch for ch in text.decode("utf-8", "replace") if ch.isdigit())
    return digits.lstrip("0") or ("0" if digits else "")


def reset(seed=None):
    rng = random.Random(seed)
    _state["problem"], _state["answer"] = _gen_problem(rng)
    _state["attempts"] = 0
    info = {"_obs_meta": {"encoding": "utf8", "kind": "math_problem"}, "attempts_left": str(MAX_ATTEMPTS)}
    return _state["problem"].encode(), info


def step(action):
    _state["attempts"] += 1
    submitted = _normalise(action if isinstance(action, (bytes, bytearray)) else str(action).encode())
    target = _state["answer"]
    correct = submitted == target
    feedback = f"correct! {target}" if correct else f"wrong, expected {target}"
    terminated = correct
    truncated = not correct and _state["attempts"] >= MAX_ATTEMPTS
    reward = 1.0 if correct else -0.01
    info = {
        "_obs_meta": {"encoding": "utf8", "kind": "math_feedback"},
        "attempt": str(_state["attempts"]),
        "verified": "true" if correct else "false",
    }
    return feedback.encode(), float(reward), bool(terminated), bool(truncated), info


def save():
    return dict(_state)


def restore(saved):
    _state.update(saved)
