#!/usr/bin/env python3
"""Vectorized-vs-single throughput comparison (EnvPool-style fan-out).

Same gridworld file twice: once plain, once with ROCKBOX_VECTORIZED=1 +
ROCKBOX_VECTORIZED_N=N so each worker step advances N sub-envs in-process
(one sandbox, one pipe round trip, one tick codec). Reports worker steps/s
and sub-steps/s for both arms. SOTA reference: EnvPool opinionated async
batching exists precisely because per-step fixed costs dominate tiny envs.

Usage: python3 scripts/bench_vectorized_compare.py [workers=8] [batches=25] [n=4]
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
SRC = open(os.path.join(os.path.dirname(os.path.abspath(__file__)),
                        "..", "priv", "samples", "rl", "vectorized_gridworld.py")).read()


def http(method, path, body=None, timeout=120):
    req = urllib.request.Request(
        URL + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {TOKEN}"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def run_arm(workers, batches, extra_env, tag, action_len=1):
    # VectorEnv consumes one action byte per sub-env, so vectorized actions
    # are N bytes wide; single-step actions stay 1 byte.
    actions = [base64.b64encode(bytes([(i + j) % 4 for j in range(action_len)])).decode()
               for i in range(32)]
    lock = threading.Lock()
    totals = {"steps": 0}
    errors = []

    def worker(wid):
        settings = {
            "language": "python", "runtime": "python-base", "entrypoint": "gridworld.py",
            "files": [{"path": "gridworld.py", "content": SRC}],
            "determinism": {"seed": wid}, "limits": {"wall_ms": 120000},
            "env": dict(extra_env),
        }
        try:
            eid = http("POST", "/api/rl/episodes", {"settings": settings})["episode_id"]
        except Exception as e:
            with lock:
                errors.append(repr(e))
            return
        try:
            for _ in range(batches):
                try:
                    ticks = http("POST", f"/api/rl/episodes/{eid}/steps", {"actions": actions})
                except Exception as e:
                    with lock:
                        errors.append(repr(e))
                    break
                with lock:
                    totals["steps"] += len(ticks.get("ticks", []))
        finally:
            try:
                http("DELETE", f"/api/rl/episodes/{eid}")
            except Exception:
                pass

    t0 = time.perf_counter()
    threads = [threading.Thread(target=worker, args=(w,)) for w in range(workers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.perf_counter() - t0
    return {"tag": tag, "worker_steps": totals["steps"], "wall_s": round(wall, 2),
            "worker_steps_per_s": round(totals["steps"] / wall, 1),
            "errors": len(errors), "first_errors": errors[:2]}


def main():
    workers = int(sys.argv[1]) if len(sys.argv) > 1 else 8
    batches = int(sys.argv[2]) if len(sys.argv) > 2 else 25
    n = int(sys.argv[3]) if len(sys.argv) > 3 else 4
    single = run_arm(workers, batches, {}, f"single(N=1)x{workers}")
    vec = run_arm(workers, batches, {"ROCKBOX_VECTORIZED": "1", "ROCKBOX_VECTORIZED_N": str(n)},
                  f"vectorized(N={n})x{workers}", action_len=n)
    vec["sub_steps_per_s"] = round(vec["worker_steps_per_s"] * n, 1)
    single["sub_steps_per_s"] = single["worker_steps_per_s"]
    print(json.dumps({
        "single": single, "vectorized": vec,
        "substep_speedup": round(vec["sub_steps_per_s"] / max(single["sub_steps_per_s"], 1), 2),
    }, indent=2))


if __name__ == "__main__":
    main()
