# rockbox_sdk

Thin stdlib-only Python client for the Rockbox sandbox API.

```bash
pip install -e sdk/python   # or just put rockbox_sdk/ on sys.path
```

## One-shot execution

```python
from rockbox_sdk import Rockbox

# Production: use an API key minted via /api/admin (see README "Auth")
rb = Rockbox("http://localhost:4000", token="rb_OiYvy...")  # or dev token in dev/test
out = rb.execute(language="python",
                 files={"main.py": "print(2+2)"},
                 wall_ms=5000)
print(out["output"])  # -> "4\n"
```

## RL episodes (Gymnasium-style, batched stepping)

```python
env_src = open("priv/samples/rl/gridworld.py").read()

with rb.episode(env_src, seed=42) as ep:
    print(ep.observation.hex())          # reset observation
    ticks = ep.steps([1, 3, 3, 1, 2])    # one round trip for all five
    total = sum(t["reward"] for t in ticks)
```

Episodes are destroyed on context exit. Deterministic seeding is passed
through to the env's `reset(seed=...)`.

## Usage metering

```python
rb.usage()  # {"workspace_id": ..., "requests_total": ..., "rl_steps_total": ..., "in_flight": ...}
```
