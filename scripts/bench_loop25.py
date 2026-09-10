#!/usr/bin/env python3
"""Loop25 evidence: worker-tick pack cost + embedded shim syntax check.

1. Times struct-based binary tick packing vs json.dumps for an identical
   logical tick (equivalent constructs of what the shim emits / emitted).
2. Extracts the real embedded `python_shim()` template from
   core/crates/engine/src/modes/rl.rs, unescapes the `format!` doubling,
   and `ast.parse`s it — so Python syntax breaks fail here, not in Lima.

Run: python3 scripts/bench_loop25.py (from repo root).
"""

import ast
import json
import pathlib
import struct
import timeit

ROOT = pathlib.Path(__file__).resolve().parent.parent
RL_RS = ROOT / "core" / "crates" / "engine" / "src" / "modes" / "rl.rs"


def extract_shim() -> str:
    text = RL_RS.read_text()
    start_marker = 'r#"import importlib.util'
    start = text.index(start_marker) + len('r#"')
    end_marker = '\n"#,\n'
    end = text.index(end_marker, start)
    # `format!` escapes literal braces by doubling them.
    return text[start:end].replace("{{", "{").replace("}}", "}")


def check_shim_syntax() -> None:
    src = extract_shim()
    assert "def _pack_tick" in src, "shim missing binary tick encoder"
    assert "RB1T" in src, "shim missing tick magic"
    ast.parse(src)
    print("shim syntax: OK (embedded python_shim parses)")


def bench_pack() -> None:
    info = {"steps": "3", "_obs_meta": '{"dtype":"uint8"}'}
    tick = {
        "reward": 1.5,
        "done": False,
        "terminated": False,
        "truncated": True,
        "info": info,
    }

    def pack_bin():
        flags = 4  # truncated
        out = [b"RB1T", struct.pack("<d", tick["reward"]), b"\x00"]
        items = list(tick["info"].items())
        out.append(struct.pack("<I", len(items)))
        for k, v in items:
            kb, vb = k.encode(), v.encode()
            out.append(struct.pack("<I", len(kb)))
            out.append(kb)
            out.append(struct.pack("<I", len(vb)))
            out.append(vb)
        out[2] = bytes((flags,))
        return b"".join(out)

    def pack_json():
        return json.dumps(tick).encode() + b"\x00" + b"OBS"

    assert pack_bin().startswith(b"RB1T")
    n = 200_000
    t_json = min(timeit.repeat(pack_json, number=n, repeat=5)) / n
    t_bin = min(timeit.repeat(pack_bin, number=n, repeat=5)) / n
    print(f"shim pack: json {t_json * 1e6:.3f}µs/op, binary {t_bin * 1e6:.3f}µs/op")


if __name__ == "__main__":
    check_shim_syntax()
    bench_pack()
