#!/usr/bin/env python3
"""A/B the warm-worker path on ONE engine: cold first episode vs claimed
successors. Reports create latency (with a pause so the prespawn is ready)
and warm single-step / batch latency for both worker kinds.
Usage: python3 scripts/bench_prespawn_ab.py"""
import base64
import json
import os
import time
import urllib.request

URL = os.environ.get("ROCKBOX_URL", "http://localhost:4000")
H = {"authorization": f"Bearer {os.environ.get('ROCKBOX_TOKEN', 'token-ws_pro_demo-pro')}",
     "content-type": "application/json"}
ENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "priv", "samples", "rl", "gridworld.py")


def http(m, p, b=None):
    r = urllib.request.Request(URL + p, data=json.dumps(b).encode() if b else None, method=m, headers=H)
    with urllib.request.urlopen(r, timeout=60) as x:
        return json.loads(x.read())


S = {"language": "python", "runtime": "python-base", "entrypoint": "gridworld.py",
     "files": [{"path": "gridworld.py", "content": open(ENV).read()}],
     "determinism": {"seed": 1}, "limits": {"wall_ms": 30000}}
A = [base64.b64encode(bytes([i % 4])).decode() for i in range(64)]


def p50(xs):
    xs = sorted(xs)
    return xs[len(xs) // 2]


def measure(eid):
    st = []
    for i in range(100):
        t0 = time.perf_counter()
        http("POST", f"/api/rl/episodes/{eid}/step", {"action": A[i % 4]})
        st.append((time.perf_counter() - t0) * 1000)
    bt = []
    for _ in range(5):
        t0 = time.perf_counter()
        http("POST", f"/api/rl/episodes/{eid}/steps", {"actions": A})
        bt.append((time.perf_counter() - t0) * 1000 / 64)
    return p50(st), p50(bt)


rows = []
for i in range(6):
    if i:
        time.sleep(0.05)  # let the background prespawn finish
    t0 = time.perf_counter()
    e = http("POST", "/api/rl/episodes", {"settings": S})
    create = (time.perf_counter() - t0) * 1000
    step, batch = measure(e["episode_id"])
    kind = "cold" if i == 0 else "claimed"
    rows.append((kind, e["vm_id"], create, step, batch))
    http("DELETE", "/api/rl/episodes/" + e["episode_id"])
for kind, vm, c, s, b in rows:
    print(f"{kind:8s} {vm:6s} create={c:5.1f}ms  step_p50={s:5.2f}ms  batch64_per_step={b:5.3f}ms")
