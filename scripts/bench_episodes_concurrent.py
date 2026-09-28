#!/usr/bin/env python3
"""Concurrent-episode scaling benchmark for the Rockbox RL path.

Spawns N parallel training-worker simulations: each opens an episode, steps it
in batches of 32 until done, closes it. Reports wall throughput (steps/s and
episodes/s) and step-latency percentiles across all workers.

Usage: python3 scripts/bench_episodes_concurrent.py [n_workers] [seconds]
"""
import base64
import json
import os
import sys
import threading
import time
import urllib.request

URL = os.environ.get("ROCKBOX_URL", "http://localhost:4000")
TOKEN = os.environ.get("ROCKBOX_TOKEN", "token-ws_pro_demo-pro")

ENV_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "..", "priv", "samples", "rl", "cartpole_lite.py")


def http(method, path, body=None, timeout=60):
    req = urllib.request.Request(
        URL + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"authorization": f"Bearer {TOKEN}", "content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def main():
    n_workers = int(sys.argv[1]) if len(sys.argv) > 1 else 16
    dur = float(sys.argv[2]) if len(sys.argv) > 2 else 20.0

    src = open(ENV_PATH).read()
    settings = {
        "language": "python",
        "runtime": "python-base",
        "entrypoint": "cartpole_lite.py",
        "files": [{"path": "cartpole_lite.py", "content": src}],
        "determinism": {"seed": None},
        "limits": {"wall_ms": 30000},
    }
    actions = [base64.b64encode(bytes([i % 2])).decode() for i in range(32)]

    lats = []
    lock = threading.Lock()
    totals = {"episodes": 0, "steps": 0}
    stop = time.perf_counter() + dur
    errors = []

    def worker(wid):
        eps = 0
        steps = 0
        local = []
        while time.perf_counter() < stop:
            try:
                settings["determinism"]["seed"] = wid * 100000 + eps
                resp = http("POST", "/api/rl/episodes", {"settings": settings})
            except Exception as e:
                with lock:
                    errors.append(repr(e))
                continue
            eid = resp["episode_id"]
            try:
                while time.perf_counter() < stop:
                    t0 = time.perf_counter()
                    try:
                        ticks = http("POST", f"/api/rl/episodes/{eid}/steps", {"actions": actions})
                    except Exception as e:
                        with lock:
                            errors.append(repr(e))
                        break
                    dt = (time.perf_counter() - t0) * 1000
                    n = len(ticks.get("ticks", []))
                    steps += n
                    local.append(dt)
                    dones = sum(1 for t in ticks["ticks"] if t.get("terminated") or t.get("truncated"))
                    if dones or n == 0:
                        break
                eps += 1
            finally:
                try:
                    http("DELETE", f"/api/rl/episodes/{eid}")
                except Exception as e:
                    with lock:
                        errors.append(repr(e))
        with lock:
            totals["episodes"] += eps
            totals["steps"] += steps
            lats.extend(local)

    threads = [threading.Thread(target=worker, args=(w,)) for w in range(n_workers)]
    t0 = time.perf_counter()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.perf_counter() - t0

    lats.sort()
    n = len(lats)
    pct = lambda q: round(lats[min(n - 1, int(n * q))], 2) if n else 0
    print(json.dumps({
        "workers": n_workers,
        "duration_s": round(wall, 2),
        "episodes_completed": totals["episodes"],
        "batch_requests": n,
        "env_steps_total": totals["steps"],
        "episode_throughput_per_s": round(totals["episodes"] / wall, 1),
        "step_throughput_per_s": round(totals["steps"] / wall, 1),
        "batch_p50_ms": pct(0.5), "p90_ms": pct(0.9),
        "p99_ms": pct(0.99), "max_ms": round(lats[-1], 2) if n else 0,
        "errors": len(errors),
        "first_errors": errors[:3],
    }, indent=2))


if __name__ == "__main__":
    main()
