#!/usr/bin/env python3
"""Episode fork benchmark: branch one live episode into N children.

Measures wall latency of POST /api/rl/episodes/:id/fork for N in FORK_NS
against creating N fresh episodes in parallel (the alternative today), and
verifies fork semantics: every child's next observation equals the parent's
for the same action (same branch point), then diverges under different
actions. Published comparison: Morph Infinibranch branches a VM in <250 ms.

Usage: python3 scripts/bench_fork.py   (ROCKBOX_URL / ROCKBOX_TOKEN env)
"""
import base64
import json
import os
import statistics
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

URL = os.environ.get("ROCKBOX_URL", "http://localhost:4000")
TOKEN = os.environ.get("ROCKBOX_TOKEN", "token-ws_pro_demo-pro")
FORK_NS = [int(x) for x in os.environ.get("FORK_NS", "1,4,16").split(",")]
ROUNDS = int(os.environ.get("ROUNDS", "5"))
ENV_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "..", "priv", "samples", "rl", "gridworld.py")


def http(method, path, body=None, timeout=60):
    req = urllib.request.Request(
        URL + path, data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"authorization": f"Bearer {TOKEN}", "content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def b64(a):
    return base64.b64encode(bytes([a])).decode()


def settings():
    return {"language": "python", "runtime": "python-base", "entrypoint": "gridworld.py",
            "files": [{"path": "gridworld.py", "content": open(ENV_PATH).read()}],
            "determinism": {"seed": 1}, "limits": {"wall_ms": 30000}}


def create():
    return http("POST", "/api/rl/episodes", {"settings": settings()})["episode_id"]


def step(eid, a):
    return http("POST", f"/api/rl/episodes/{eid}/step", {"action": b64(a)})


def destroy(eid):
    try:
        http("DELETE", f"/api/rl/episodes/{eid}")
    except Exception:
        pass


def pct(xs):
    xs = sorted(xs)
    return {"p50_ms": round(xs[len(xs) // 2], 1), "max_ms": round(xs[-1], 1), "n": len(xs)}


def main():
    report = {}
    # Semantics check once, on a 3-way fork.
    parent = create()
    for a in (3, 3, 1):
        step(parent, a)
    fork = http("POST", f"/api/rl/episodes/{parent}/fork", {"n": 3})
    kids = [c["episode_id"] for c in fork["children"]]
    assert all("error" not in c for c in fork["children"]), fork
    assert fork["steps"] == "3", fork
    p_next = step(parent, 3)
    k_next = [step(k, 3) for k in kids]
    assert all(t["observation"] == p_next["observation"] for t in k_next), "children diverged from parent at branch point"
    assert all(t["info"].get("steps") == "4" for t in k_next), [t["info"] for t in k_next]
    assert any(t["info"].get("resumed") == "true" for t in k_next), "child did not report resumed"
    p_div = step(parent, 1)
    k_div = step(kids[0], 2)
    assert p_div["observation"] != k_div["observation"], "children must be independent"
    report["semantics"] = "ok (3 children match parent at branch point, then diverge)"
    for k in kids:
        destroy(k)
    destroy(parent)

    for n in FORK_NS:
        fork_lat, fresh_lat = [], []
        for _ in range(ROUNDS):
            parent = create()
            step(parent, 3)
            t0 = time.perf_counter()
            res = http("POST", f"/api/rl/episodes/{parent}/fork", {"n": n})
            fork_lat.append((time.perf_counter() - t0) * 1000)
            errs = [c for c in res["children"] if "error" in c]
            assert not errs, errs[:2]
            for c in res["children"]:
                destroy(c["episode_id"])
            destroy(parent)

            t0 = time.perf_counter()
            with ThreadPoolExecutor(max_workers=n) as ex:
                fresh = list(ex.map(lambda _: create(), range(n)))
            fresh_lat.append((time.perf_counter() - t0) * 1000)
            for e in fresh:
                destroy(e)
        report[f"fork_n{n}"] = pct(fork_lat)
        report[f"fresh_create_n{n}"] = pct(fresh_lat)
        report[f"fork_n{n}"]["per_child_ms"] = round(statistics.median(fork_lat) / n, 2)

    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
