#!/usr/bin/env python3
"""Loop27 e2e: JSON vs msgpack stepping transport on 64KB observations.

Runs from anywhere with HTTP access to the stack (Lima forwards :4000).
Creates one big-obs episode, times identical 32-action batches over plain
JSON and over the SDK's msgpack transport, deletes the episode.
"""
import base64
import json
import os
import sys
import time
import urllib.request

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "sdk", "python"))

URL = os.environ.get("ROCKBOX_URL", "http://localhost:4000")
TOKEN = os.environ.get("ROCKBOX_TOKEN", "token-ws_pro_demo-pro")

ENV_SRC = """
OB = b"K" * 65536

def reset(seed=None):
    return OB, {}

def step(action):
    return OB, 1.0, False, False, {}
"""

N_BATCHES = int(os.environ.get("N_BATCHES", "8"))
BATCH = 32


def http(method, path, body=None, headers=None, timeout=120):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        URL + path, data=data, method=method,
        headers={"Content-Type": "application/json",
                 "Authorization": f"Bearer {TOKEN}", **(headers or {})},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, r.headers.get_content_type(), r.read()


def main():
    settings = {
        "language": "python", "runtime": "python-base", "entrypoint": "bigobs.py",
        "files": [{"path": "bigobs.py", "content": ENV_SRC}],
        "determinism": {"seed": 7}, "limits": {"wall_ms": 60000},
    }
    _, _, raw = http("POST", "/api/rl/episodes", {"settings": settings})
    eid = json.loads(raw)["episode_id"]
    actions = [base64.b64encode(bytes([i % 2])).decode() for i in range(BATCH)]
    try:
        # Warmup (worker boot, caches) on both paths.
        http("POST", f"/api/rl/episodes/{eid}/steps", {"actions": actions})
        from rockbox_sdk import Rockbox
        sdk = Rockbox(URL, token=TOKEN)

        def time_json():
            t0 = time.perf_counter()
            _, _, resp_raw = http("POST", f"/api/rl/episodes/{eid}/steps", {"actions": actions})
            dt = (time.perf_counter() - t0) * 1000
            ticks = json.loads(resp_raw)["ticks"]
            assert len(ticks) == BATCH and len(base64.b64decode(ticks[0]["observation"])) == 65536
            return dt

        def time_mp():
            # SDK steps() speaks msgpack; bypass episode registry via raw id.
            import rockbox_sdk as S
            ep = S._Episode(sdk, eid, {})
            t0 = time.perf_counter()
            ticks = ep.steps([bytes([i % 2]) for i in range(BATCH)])
            dt = (time.perf_counter() - t0) * 1000
            assert len(ticks) == BATCH and len(ticks[0]["observation_bytes"]) == 65536
            return dt

        j = sorted(time_json() for _ in range(N_BATCHES))
        m = sorted(time_mp() for _ in range(N_BATCHES))
        med = lambda xs: xs[len(xs) // 2]
        print(json.dumps({
            "obs": "64KB", "batch": BATCH, "n": N_BATCHES,
            "json_batch_ms_p50": round(med(j), 2),
            "json_per_step_ms": round(med(j) / BATCH, 3),
            "msgpack_batch_ms_p50": round(med(m), 2),
            "msgpack_per_step_ms": round(med(m) / BATCH, 3),
            "speedup": round(med(j) / med(m), 2),
        }, indent=2))
    finally:
        http("DELETE", f"/api/rl/episodes/{eid}")


if __name__ == "__main__":
    main()
