"""Rockbox sample: multi-turn tool-use agent environment (agentic RL).

The agent must solve a task by calling tools over multiple turns, in the
style of LLM function-calling rollouts. Actions are utf-8 JSON commands:

    {"tool": "calc", "expr": "12 * 8"}     -> {"result": 96}
    {"tool": "cmp",  "a": 96, "b": "95"}   -> {"result": "gt"}
    {"tool": "answer", "relation": "eq"|"gt"|"lt"}  -> final answer

Task (per seed): evaluate `A op B` mentally via calc, then report whether the
result compares to C as eq/gt/lt. Reward: +1 correct final answer with
efficiency shaping (-0.05 per extra turn), -0.02 per malformed call.
Terminated on a correct `answer`; truncated after MAX_TURNS.

    reset([seed=...])   -> (task_text, info)
    step(json_text)     -> (tool_output_text, reward, terminated, truncated, info)
"""

import json
import random

MAX_TURNS = 8

_state = {"a": 0, "b": 0, "c": 0, "turns": 0}


def _truth(a, b, c):
    return "eq" if a == c else ("gt" if a > c else "lt")


def reset(seed=None):
    rng = random.Random(seed)
    _state["a"] = rng.randint(2, 20)
    _state["b"] = rng.randint(2, 12)
    _state["c"] = _state["a"] * _state["b"] + rng.choice([-5, -1, 0, 3, 7])
    _state["turns"] = 0
    task = f"Compute {_state['a']} * {_state['b']} using the calc tool, then answer whether it is eq/gt/lt to {_state['c']}."
    return task.encode(), {"_obs_meta": {"encoding": "utf8", "kind": "tool_task"}, "max_turns": str(MAX_TURNS)}


def step(action):
    _state["turns"] += 1
    try:
        cmd = json.loads(action.decode("utf-8", "replace"))
        tool = cmd.get("tool")
    except Exception:  # noqa: BLE112
        tool = None
    obs_meta = {"encoding": "utf8", "kind": "tool_output"}

    if tool == "calc":
        expr = str(cmd.get("expr", ""))
        if set(expr) <= set("0123456789+-*() ") and expr:
            val = eval(expr, {"__builtins__": {}})  # noqa: S307 — arithmetic only, char-allowlisted
            out = json.dumps({"result": val})
        else:
            out = json.dumps({"error": "bad expr"})
    elif tool == "cmp":
        try:
            a, b = float(cmd.get("a")), float(cmd.get("b"))
            out = json.dumps({"result": "gt" if a > b else ("lt" if a < b else "eq")})
        except (TypeError, ValueError):
            out = json.dumps({"error": "cmp needs numeric a,b"})
    elif tool == "answer":
        relation = cmd.get("relation")
        correct = relation == _truth(_state["a"], _state["b"], _state["c"])
        reward = 1.0 - 0.05 * (_state["turns"] - 2) if _state["turns"] >= 2 else 1.0
        if not correct:
            reward = -0.1
        info = {"verified": "true" if correct else "false",
                "_obs_meta": {"encoding": "utf8", "kind": "grade"},
                "turns": str(_state["turns"])}
        return f"graded: {'correct' if correct else 'wrong'}".encode(), float(reward), bool(correct), False, info
    else:
        out = json.dumps({"error": "unknown tool"})
        obs_meta = None  # malformed call: plain output

    shaped = -0.02 if tool not in ("calc", "cmp") else 0.0
    truncated = _state["turns"] >= MAX_TURNS
    info = {"turns": str(_state["turns"])}
    if obs_meta:
        info["_obs_meta"] = obs_meta
    payload = out.encode()
    return payload, shaped, False, bool(truncated), info


def save():
    return dict(_state)


def restore(saved):
    _state.update(saved)
