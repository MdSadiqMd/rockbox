"""Rockbox RLVR sample: code-that-passes-tests (SWE-lite, verifiable rewards).

The agent submits a Python function body; the environment EXECUTES it inside
this sandboxed worker and runs hidden unit tests against it. Reward = fraction
of tests passed. Untrusted generated code runs inside Rockbox's 10-layer
sandbox — that isolation is the whole point of running this env here.

    reset([seed=...])   -> (task_text, info)
    step(source_text)   -> (test_report_text, reward, terminated, truncated, info)

One submission per episode (terminated immediately after scoring); a fresh
seed picks a different task from the taskset.
"""

import random

TASKS = [
    {
        "name": "add",
        "doc": "Return the sum of ints a and b.",
        "tests": [("add(2, 3)", 5), ("add(-1, 1)", 0), ("add(0, 0)", 0)],
    },
    {
        "name": "is_palindrome",
        "doc": "Return True if string s reads the same forwards and backwards.",
        "tests": [("is_palindrome('abba')", True), ("is_palindrome('abc')", False),
                  ("is_palindrome('')", True)],
    },
    {
        "name": "clamp",
        "doc": "Clamp value x into [lo, hi].",
        "tests": [("clamp(5, 0, 10)", 5), ("clamp(-3, 0, 10)", 0), ("clamp(99, 0, 10)", 10)],
    },
]

MAX_ATTEMPTS = 2
_state = {"task": None, "attempts": 0}


def _render_task(task):
    return (
        f"Implement `{task['name']}`.\n"
        f"Doc: {task['doc']}\n"
        f"Submit the function body as `def {task['name']}(...): ...` source text."
    ).encode()


def reset(seed=None):
    rng = random.Random(seed)
    _state["task"] = TASKS[rng.randrange(len(TASKS))]
    _state["attempts"] = 0
    info = {"_obs_meta": {"encoding": "utf8", "kind": "code_task"}, "attempts_left": str(MAX_ATTEMPTS)}
    return _render_task(_state["task"]), info


def _grade(source_text):
    src = source_text.decode("utf-8", "replace")
    namespace = {}
    passed, total, error = 0, 0, None
    try:
        exec(src, namespace)  # noqa: S102 — untrusted code, this IS the sandbox
        fn = namespace.get(_state["task"]["name"])
        if not callable(fn):
            return 0.0, 0, f"name {_state['task']['name']} not defined or not callable"
        for expr, expected in _state["task"]["tests"]:
            total += 1
            try:
                got = eval(expr, {"__fn": fn, _state["task"]["name"]: fn})  # noqa: S307
                if got == expected:
                    passed += 1
            except Exception as e:  # noqa: BLE112
                error = f"{type(e).__name__}: {e}"
        return passed / total if total else 0.0, passed, error
    except Exception as e:  # noqa: BLE112
        return 0.0, 0, f"{type(e).__name__}: {e}"


def step(action):
    _state["attempts"] += 1
    reward_frac, passed, error = _grade(action)
    task = _state["task"]
    report = f"{passed}/{len(task['tests'])} tests passed" + (f"; last error: {error}" if error else "")
    terminated = reward_frac == 1.0
    truncated = not terminated and _state["attempts"] >= MAX_ATTEMPTS
    reward = reward_frac - 0.01
    info = {
        "_obs_meta": {"encoding": "utf8", "kind": "test_report"},
        "passed": str(passed),
        "total": str(len(task["tests"])),
        "verified": "true" if terminated else "false",
    }
    return report.encode(), float(reward), bool(terminated), bool(truncated), info


def save():
    return dict(_state)


def restore(saved):
    _state.update(saved)
