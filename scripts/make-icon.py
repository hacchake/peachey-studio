#!/usr/bin/env python3
"""Draws the Peachey Studio icon (a citrus disc with a curl of peel) as PNGs.

Pure Python, no dependencies: signed distances with analytic edge
antialiasing, written with zlib. Usage: make-icon.py OUT_DIR
"""
import math
import struct
import sys
import zlib

BG_TOP = (0x23, 0x29, 0x33)
BG_BOTTOM = (0x14, 0x17, 0x1D)
AMBER = (0xF2, 0xB6, 0x6D)
AMBER_DARK = tuple(int(c * 0.82) for c in AMBER)
AMBER_LIGHT = tuple(int(c + (255 - c) * 0.15) for c in AMBER)


def clamp(x, lo=0.0, hi=1.0):
    return lo if x < lo else hi if x > hi else x


def mix(a, b, t):
    return tuple(a[i] + (b[i] - a[i]) * t for i in range(3))


def sd_round_box(px, py, cx, cy, hw, hh, r):
    qx = abs(px - cx) - hw + r
    qy = abs(py - cy) - hh + r
    return min(max(qx, qy), 0.0) + math.hypot(max(qx, 0.0), max(qy, 0.0)) - r


def sd_arc(px, py, cx, cy, radius, thick, start, sweep):
    x, y = px - cx, py - cy
    ring = abs(math.hypot(x, y) - radius) - thick * 0.5
    ang = math.atan2(x, -y)
    rel = (ang - start) % (2 * math.pi)
    if rel <= sweep:
        return ring
    e0 = (radius * math.sin(start), -radius * math.cos(start))
    e1 = (radius * math.sin(start + sweep), -radius * math.cos(start + sweep))
    return min(math.hypot(x - e0[0], y - e0[1]), math.hypot(x - e1[0], y - e1[1])) - thick * 0.5


def render(size):
    s = size / 1024.0
    rows = []
    for j in range(size):
        row = bytearray([0])
        py = j + 0.5
        for i in range(size):
            px = i + 0.5
            # macOS-style tile with margin.
            d_tile = sd_round_box(px, py, size / 2, size / 2, 412 * s, 412 * s, 92 * s)
            a = clamp(0.5 - d_tile)
            if a <= 0.0:
                row += b"\0\0\0\0"
                continue
            col = mix(BG_TOP, BG_BOTTOM, clamp((py - 100 * s) / (824 * s)))
            cx, cy = size / 2 - 18 * s, size / 2 + 22 * s
            d = math.hypot(px - cx, py - cy)
            # Disc, inner flesh, and the bite where the peel lifts away.
            col = mix(col, AMBER, clamp(0.5 - (d - 300 * s)))
            col = mix(col, AMBER_DARK, clamp(0.5 - (d - 232 * s)))
            # Segment lines of the cut fruit.
            if d < 236 * s:
                ang = math.atan2(px - cx, -(py - cy))
                seg = (ang % (math.pi / 4)) - math.pi / 8
                # Distance to the nearest spoke at multiples of 45 degrees.
                dist = d * abs(math.sin(math.pi / 8 - abs(seg)))
                col = mix(col, AMBER, clamp(0.5 - (dist - 7 * s)) * clamp((d - 40 * s) / (20 * s)))
                col = mix(col, AMBER, clamp(0.5 - (d - 34 * s)))
            col = mix(col, mix(BG_TOP, BG_BOTTOM, 0.35), clamp(0.5 - sd_arc(px, py, cx, cy, 300 * s, 70 * s, 0.35, 1.25)))
            # The curl.
            col = mix(col, AMBER_LIGHT, clamp(0.5 - sd_arc(px, py, cx + 238 * s, cy - 226 * s, 96 * s, 52 * s, 3.5, 2.5)))
            row += bytes([int(col[0]), int(col[1]), int(col[2]), int(255 * a)])
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    for size in (1024, 512, 256, 128, 64, 32, 16):
        with open(f"{out}/peel-{size}.png", "wb") as f:
            f.write(render(size))
        print(f"wrote {out}/peel-{size}.png")
