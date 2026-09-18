"""Minimal msgpack *decoder*, stdlib only.

The Rockbox server answers `Accept: application/msgpack` RL stepping calls
with raw-bytes ticks (no base64), which is ~155x cheaper to produce than the
JSON equivalent for 64 KB observations (host-measured: 3.1 ms -> 0.02 ms per
32-tick batch). The SDK stays dependency-free by vendoring just the decode
half of msgpack here (~90 lines); requests remain plain JSON.

Scope: exactly what the server emits on the stepping path — nil, bools,
ints uint64, float32/64, str/bin/array/map (8/16/32-bit lengths). Extension
types never appear on this path and raise loudly if they ever do, so a
server-side encoding change fails closed instead of mis-decoding.

Bytes-vs-text rule (mirrors `msgpack.RawToString` care): `str`-family values
that are valid UTF-8 decode to `str` (map keys, info values, ids); invalid
UTF-8 `str` payloads (raw observations packed as strings) and every `bin`
fall back to `bytes`. Callers normalise observations with `to_bytes`, which
round-trips both cases exactly.
"""

from __future__ import annotations

import struct

__all__ = ["MsgpackError", "unpackb", "to_bytes"]


class MsgpackError(ValueError):
    """Raised on truncated input, trailing bytes, or unsupported types."""


class _Reader:
    __slots__ = ("_b", "_i")

    def __init__(self, buf: bytes):
        self._b = buf
        self._i = 0

    def take(self, n: int) -> bytes:
        end = self._i + n
        if end > len(self._b):
            raise MsgpackError(f"truncated input at offset {self._i} (need {n})")
        chunk = self._b[self._i : end]
        self._i = end
        return chunk

    def u8(self) -> int:
        return self.take(1)[0]


def _text(raw: bytes):
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        return raw


def _unpack(r: _Reader):
    b = r.u8()
    if b <= 0x7F:  # positive fixint
        return b
    if b >= 0xE0:  # negative fixint
        return b - 0x100
    if 0xA0 <= b <= 0xBF:  # fixstr
        return _text(r.take(b & 0x1F))
    if 0x90 <= b <= 0x9F:  # fixarray
        return [_unpack(r) for _ in range(b & 0x0F)]
    if 0x80 <= b <= 0x8F:  # fixmap
        return {_unpack(r): _unpack(r) for _ in range(b & 0x0F)}
    if b == 0xC0:
        return None
    if b == 0xC2:
        return False
    if b == 0xC3:
        return True
    if b == 0xCA:  # float32
        return struct.unpack(">f", r.take(4))[0]
    if b == 0xCB:  # float64
        return struct.unpack(">d", r.take(8))[0]
    if b == 0xCC:
        return r.u8()
    if b == 0xCD:
        return struct.unpack(">H", r.take(2))[0]
    if b == 0xCE:
        return struct.unpack(">I", r.take(4))[0]
    if b == 0xCF:
        return struct.unpack(">Q", r.take(8))[0]
    if b == 0xD0:
        return struct.unpack(">b", r.take(1))[0]
    if b == 0xD1:
        return struct.unpack(">h", r.take(2))[0]
    if b == 0xD2:
        return struct.unpack(">i", r.take(4))[0]
    if b == 0xD3:
        return struct.unpack(">q", r.take(8))[0]
    if b == 0xD9:  # str8
        return _text(r.take(r.u8()))
    if b == 0xDA:  # str16
        return _text(r.take(struct.unpack(">H", r.take(2))[0]))
    if b == 0xDB:  # str32
        return _text(r.take(struct.unpack(">I", r.take(4))[0]))
    if b == 0xC4:  # bin8
        return bytes(r.take(r.u8()))
    if b == 0xC5:  # bin16
        return bytes(r.take(struct.unpack(">H", r.take(2))[0]))
    if b == 0xC6:  # bin32
        return bytes(r.take(struct.unpack(">I", r.take(4))[0]))
    if b == 0xDC:  # array16
        return [_unpack(r) for _ in range(struct.unpack(">H", r.take(2))[0])]
    if b == 0xDD:  # array32
        return [_unpack(r) for _ in range(struct.unpack(">I", r.take(4))[0])]
    if b == 0xDE:  # map16
        n = struct.unpack(">H", r.take(2))[0]
        return {_unpack(r): _unpack(r) for _ in range(n)}
    if b == 0xDF:  # map32
        n = struct.unpack(">I", r.take(4))[0]
        return {_unpack(r): _unpack(r) for _ in range(n)}
    raise MsgpackError(f"unsupported msgpack marker 0x{b:02x} at offset {r._i - 1}")


def unpackb(buf: bytes):
    """Decode one msgpack value; the whole buffer must be exactly one value."""
    r = _Reader(bytes(buf))
    value = _unpack(r)
    if r._i != len(r._b):
        raise MsgpackError(f"trailing bytes after msgpack value ({len(r._b) - r._i})")
    return value


def to_bytes(value) -> bytes:
    """Normalise a decoded observation to bytes (str round-trips exactly)."""
    if isinstance(value, bytes):
        return value
    if isinstance(value, str):
        return value.encode("utf-8")
    if value is None:
        return b""
    if isinstance(value, list):
        return bytes(value)
    raise MsgpackError(f"cannot interpret {type(value).__name__} as observation bytes")
