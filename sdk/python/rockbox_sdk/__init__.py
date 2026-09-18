"""Rockbox Python SDK — thin client for the sandbox-as-a-service API.

Covers the full surface: one-shot execution, persistent sessions, and
Gymnasium-style RL episodes with EnvPool-style batched stepping.

    from rockbox_sdk import Rockbox

    rb = Rockbox("http://localhost:4000", token="token-ws_pro_demo-pro")

    # One-shot execution
    result = rb.execute(language="python", files={"main.py": "print(2+2)"})

    # RL episode (context manager destroys the episode on exit)
    with rb.episode(env_source, seed=42) as ep:
        tick = ep.reset()
        ticks = ep.steps([0, 1, 3, 2])   # one round trip for all four

Zero dependencies (stdlib only), so it can vendor into any training loop.
"""

from __future__ import annotations

import base64
import json
import urllib.error
import urllib.request
from typing import Iterable, Optional

try:
    # Package installs decode msgpack stepping responses (raw-bytes ticks,
    # no base64). Single-file vendors without `_msgpack.py` stay JSON-only.
    from ._msgpack import to_bytes as _mp_to_bytes
    from ._msgpack import unpackb as _mp_unpackb
except ImportError:  # pragma: no cover - vendored single-file use
    _mp_to_bytes = None  # type: ignore[assignment]
    _mp_unpackb = None  # type: ignore[assignment]

_MSGPACK_ACCEPT = "application/msgpack"

__all__ = ["Rockbox", "RockboxError"]

DEFAULT_TIMEOUT = 60.0


class RockboxError(RuntimeError):
    def __init__(self, status: int, body: dict):
        self.status = status
        self.body = body
        super().__init__(f"rockbox api {status}: {body}")


class _Episode:
    """A live RL episode. Created by `Rockbox.episode()`."""

    def __init__(self, client: "Rockbox", episode_id: str, initial: dict):
        self.client = client
        self.id = episode_id
        self.initial = initial

    @property
    def observation(self) -> bytes:
        return base64.b64decode(self.initial.get("observation") or "")

    def step(self, action: bytes | int) -> dict:
        frame = action if isinstance(action, bytes) else bytes([action])
        return self._tick_request(
            "POST", f"/api/rl/episodes/{self.id}/step",
            {"action": base64.b64encode(frame).decode()})

    def steps(self, actions: Iterable[bytes | int]) -> list[dict]:
        frames = [a if isinstance(a, bytes) else bytes([a]) for a in actions]
        return self._tick_request(
            "POST", f"/api/rl/episodes/{self.id}/steps",
            {"actions": [base64.b64encode(f).decode() for f in frames]})["ticks"]

    def _tick_request(self, method: str, path: str, body: dict):
        """Step/steps round trip. Prefers msgpack (raw-bytes ticks, no base64);
        falls back to JSON for servers that ignore `Accept` so the return
        shape — including `observation_bytes: bytes` — never changes.
        """
        _, ctype, raw = self.client._request_raw(
            method, path, body, accept=_MSGPACK_ACCEPT)
        if _mp_unpackb is not None and "msgpack" in (ctype or ""):
            resp = _mp_unpackb(raw)
        else:
            resp = json.loads(raw)
            for t in resp["ticks"] if isinstance(resp, dict) and "ticks" in resp else [resp]:
                t["observation_bytes"] = base64.b64decode(t.get("observation") or "")
            return resp
        for t in resp["ticks"] if isinstance(resp, dict) and "ticks" in resp else [resp]:
            t["observation_bytes"] = _mp_to_bytes(t.get("observation"))
        return resp

    def metrics(self) -> dict:
        return self.client._request("GET", "/api/usage")[1]

    # ---------------------------------------------------------------- files

    def list_files(self, path: str = "/") -> list[dict]:
        _, resp = self.client._request(
            "GET", f"/api/rl/episodes/{self.id}/files?path={path}")
        return resp

    def read_file(self, path: str) -> bytes:
        _, resp = self.client._request(
            "GET", f"/api/rl/episodes/{self.id}/files/content?path={path}")
        return base64.b64decode(resp["content_b64"])

    def write_file(self, path: str, content: bytes) -> dict:
        _, resp = self.client._request(
            "PUT", f"/api/rl/episodes/{self.id}/files",
            {"path": path, "content": base64.b64encode(content).decode()})
        return resp

    def remove_file(self, path: str) -> dict:
        _, resp = self.client._request(
            "DELETE", f"/api/rl/episodes/{self.id}/files?path={path}")
        return resp

    def close(self) -> None:
        self.client._request("DELETE", f"/api/rl/episodes/{self.id}")

    def __enter__(self) -> "_Episode":
        return self

    def __exit__(self, *exc) -> None:
        try:
            self.close()
        except RockboxError:
            pass


class Rockbox:
    def __init__(self, url: str = "http://localhost:4000", token: str = "",
                 timeout: float = DEFAULT_TIMEOUT):
        self.url = url.rstrip("/")
        self.token = token
        self.timeout = timeout
        # Persistent HTTP connection pool. Stepping at sub-millisecond server
        # latency makes per-request TCP setup the dominant client-side cost
        # (~0.26 ms/step measured); a pooled keep-alive connection removes it.
        self._opener = urllib.request.build_opener()
        self._opener.addheaders = [
            ("authorization", f"Bearer {self.token}"),
            ("content-type", "application/json"),
            ("connection", "keep-alive"),
        ]

    def _request(self, method: str, path: str, body: Optional[dict] = None,
                 timeout: Optional[float] = None):
        status, _ctype, raw = self._request_raw(method, path, body, timeout)
        return status, json.loads(raw)

    def _request_raw(self, method: str, path: str, body: Optional[dict] = None,
                     timeout: Optional[float] = None, accept: Optional[str] = None):
        """Like `_request` but returns `(status, content_type, raw_bytes)`.

        Stepping calls pass `accept="application/msgpack"` so the server
        answers with raw-bytes ticks (no base64); every other call stays JSON.
        """
        headers = {"content-type": "application/json"}
        if accept:
            headers["accept"] = accept
        req = urllib.request.Request(
            self.url + path,
            data=json.dumps(body).encode() if body is not None else None,
            method=method,
            headers=headers,
        )
        try:
            with self._opener.open(req, timeout=timeout or self.timeout) as r:
                return r.status, r.headers.get_content_type(), r.read()
        except urllib.error.HTTPError as e:
            try:
                payload = json.loads(e.read())
            except Exception:
                payload = {}
            raise RockboxError(e.code, payload) from None

    # ------------------------------------------------------------- execution

    def execute(self, *, language: str, files: dict[str, str],
                entrypoint: Optional[str] = None, runtime: Optional[str] = None,
                wall_ms: int = 5000, memory_mb: Optional[int] = None,
                env: Optional[dict[str, str]] = None,
                network: Optional[str] = None) -> dict:
        settings = {
            "language": language,
            "entrypoint": entrypoint or next(iter(files)),
            "files": [{"path": p, "content": c} for p, c in files.items()],
            "limits": {"wall_ms": wall_ms, **({"memory_mb": memory_mb} if memory_mb else {})},
        }
        if runtime:
            settings["runtime"] = runtime
        if env:
            settings["env"] = env
        if network:
            settings["network"] = {"tier": network}
        _, resp = self._request("POST", "/api/execute", {"settings": settings})
        return resp

    def usage(self) -> dict:
        _, resp = self._request("GET", "/api/usage")
        return resp

    # ------------------------------------------------------------------- RL

    def start_episode(self, env_source: str, *, seed: Optional[int] = None,
                      entrypoint: str = "env.py", wall_ms: int = 30000) -> _Episode:
        settings = {
            "language": "python",
            "runtime": "python-base",
            "entrypoint": entrypoint,
            "files": [{"path": entrypoint, "content": env_source}],
            "limits": {"wall_ms": wall_ms},
        }
        if seed is not None:
            settings["determinism"] = {"seed": seed}
        _, resp = self._request("POST", "/api/rl/episodes", {"settings": settings})
        return _Episode(self, resp["episode_id"], resp.get("initial") or {})

    def episode(self, env_source: str, *, seed: Optional[int] = None,
                entrypoint: str = "env.py", wall_ms: int = 30000) -> _Episode:
        return self.start_episode(env_source, seed=seed, entrypoint=entrypoint,
                                  wall_ms=wall_ms)
