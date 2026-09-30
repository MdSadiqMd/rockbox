#!/usr/bin/env python3
"""Local competitive benchmark for Rockbox as an RL sandbox service.

Measures, on this machine (Lima VM):
  1. subprocess baseline   — no isolation, interpreter floor
  2. container-per-request — docker run per execution (the classic
                             self-hosted sandbox model; what E2B/Daytona-
                             style systems do behind their APIs)
  3. Rockbox exec          — POST /api/execute through the full stack
  4. Rockbox RL stepping   — batched steps + episode lifecycle

Published cloud numbers (E2B/Modal/Daytona docs) are included in the report
for context but are NOT measured here — different hardware/networks.

Run inside the app container:
  docker compose exec app python3 /app/scripts/bench_competitive.py
or from the host with ROCKBOX_URL pointing at the stack and DOCKER available.
"""

import json
import os
import statistics
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

URL = os.environ.get("ROCKBOX_URL", "http://localhost:4000")
TOKEN = os.environ.get("ROCKBOX_TOKEN", "token-ws_pro_demo-pro")
DOCKER_IMAGE = os.environ.get("BENCH_DOCKER_IMAGE", "python:3.13-alpine")
N_SUB = int(os.environ.get("N_SUBPROCESS", "30"))
N_DOCKER = int(os.environ.get("N_DOCKER", "12"))
N_ROCK = int(os.environ.get("N_ROCKBOX", "60"))


def http(method, path, body=None, timeout=60):
    req = urllib.request.Request(
        URL + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"authorization": f"Bearer {TOKEN}", "content-type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read())


def timed(fn, n, warmup=3):
    for _ in range(warmup):
        fn()
    xs = []
    for _ in range(n):
        t0 = time.perf_counter()
        fn()
        xs.append((time.perf_counter() - t0) * 1000)
    xs.sort()
    return {
        "n": n,
        "min_ms": round(xs[0], 2),
        "p50_ms": round(xs[len(xs) // 2], 2),
        "p95_ms": round(xs[int(n * 0.95)], 2),
        "max_ms": round(xs[-1], 2),
    }


def find_python():
    for p in ("/nix/var/nix/profiles/default/bin/python3", "/usr/local/bin/python3",
              "/usr/bin/python3"):
        if os.path.exists(p):
            return p
    return sys.executable


def bench_subprocess():
    py = find_python()
    return timed(lambda: subprocess.run(
        [py, "-S", "-c", "pass"], check=True, capture_output=True), N_SUB)


def bench_docker_per_request():
    def run():
        subprocess.run(
            ["docker", "run", "--rm", "--network=none", "--memory=256m",
             "--cpus=1", DOCKER_IMAGE, "python3", "-S", "-c", "pass"],
            check=True, capture_output=True)
    return timed(run, N_DOCKER, warmup=1)


def bench_rockbox_exec():
    payload = {
        "settings": {
            "language": "python",
            "runtime": "python-base",
            "entrypoint": "main.py",
            "files": [{"path": "main.py", "content": "print(1+1)"}],
            "limits": {"wall_ms": 5000},
        }
    }
    out = {}

    def run():
        http("POST", "/api/execute", payload)

    out["warm"] = timed(run, N_ROCK)

    # cold-ish: unique program forces a fresh compile-free path but new
    # request id; true cold (engine boot) measured separately via restarts.
    return out


def bench_rockbox_rl():
    import base64
    src = open(os.path.join(os.path.dirname(__file__), "..",
               "priv/samples/rl/gridworld.py")).read()
    resp = http("POST", "/api/rl/episodes", {
        "settings": {
            "language": "python", "runtime": "python-base",
            "entrypoint": "gridworld.py",
            "files": [{"path": "gridworld.py", "content": src}],
            "determinism": {"seed": 42},
            "limits": {"wall_ms": 30000}}})
    eid = resp["episode_id"]
    try:
        actions = [base64.b64encode(bytes([i % 4])).decode() for i in range(64)]
        batches = []
        for _ in range(5):
            t0 = time.perf_counter()
            r = http("POST", f"/api/rl/episodes/{eid}/steps", {"actions": actions})
            batches.append((time.perf_counter() - t0) * 1000)
            assert len(r["ticks"]) == 64
        batches.sort()
        lifecycle = []
        for i in range(8):
            t0 = time.perf_counter()
            e2 = http("POST", "/api/rl/episodes", {
                "settings": {
                    "language": "python", "runtime": "python-base",
                    "entrypoint": "gridworld.py",
                    "files": [{"path": "gridworld.py", "content": src}],
                    "determinism": {"seed": 100 + i},
                    "limits": {"wall_ms": 30000}}})
            http("POST", f"/api/rl/episodes/{e2['episode_id']}/steps",
                 {"actions": actions[:4]})
            http("DELETE", f"/api/rl/episodes/{e2['episode_id']}")
            lifecycle.append((time.perf_counter() - t0) * 1000)
        lifecycle.sort()
        return {
            "batch64_wall_ms_p50": round(batches[2], 2),
            "batch64_per_step_ms_p50": round(batches[2] / 64, 3),
            "lifecycle_create_step_destroy_p50_ms": round(lifecycle[4], 2),
        }
    finally:
        http("DELETE", f"/api/rl/episodes/{eid}")


def main():
    print(f"benchmarking against {URL} (docker image: {DOCKER_IMAGE})")
    report = {"measured_on": time.strftime("%Y-%m-%dT%H:%M:%S"), "url": URL}

    print("  subprocess baseline…")
    report["subprocess_no_isolation"] = bench_subprocess()
    print("  container-per-request (docker)…")
    try:
        report["docker_container_per_request"] = bench_docker_per_request()
    except Exception as e:
        report["docker_container_per_request"] = {"error": repr(e)}
    print("  rockbox exec…")
    report["rockbox_exec"] = bench_rockbox_exec()
    print("  rockbox rl…")
    report["rockbox_rl"] = bench_rockbox_rl()

    report["published_third_party_context"] = {
        "note": "Vendor-published figures, different hardware — context only, not measured here",
        "e2b_cold_start_ms": [150, 300],
        "daytona_cold_start_ms": [27, 90],
        "modal_cpu_cold_start_ms": [100, 500],
        "envpool_atari_fps_per_core_approx": 4000,
    }

    out = os.path.join(os.path.dirname(__file__), "..", ".bench_competitive.json")
    with open(out, "w") as f:
        json.dump(report, f, indent=2)
    print(json.dumps({k: v for k, v in report.items()
                      if k != "published_third_party_context"}, indent=2))
    print(f"report → {os.path.abspath(out)}")


if __name__ == "__main__":
    main()
