#!/usr/bin/env python3
"""Draw the tokumai app icon: the striped mark on the ink ground, at any size.

The icon was a hand-made PNG whose mark sat small inside a wide dark border. Redrawing it
from the same geometry the site and the app use (the eight rounded bars of the wordmark's
logo) makes the size of the mark a number instead of a Photoshop decision — MARK_SCALE
below — and lets every platform's icon be regenerated from one command.

    scripts/make-icon.py [size] [out.png]      # default 1024, src-tauri/icons/icon.png

Then `npm run tauri icon src-tauri/icons/icon.png` fans it out to every platform, and
scripts/ios-icons-opaque.py takes the alpha back off the iOS set.

Pure standard library: no PIL on this machine. Supersampled 4x, so the bars keep clean
edges at 20px as well as at 1024.
"""

import struct
import sys
import zlib

# The mark, in the 100x100 box the site's <svg> uses: eight bars, alternating bone/violet.
# x, y, w, h — the corner radius is half the bar height, as in the SVG (rx 4.5 of h 9).
BARS = [
    (19, 28, 27, 9, "bone"), (49, 28, 31, 9, "acc"),
    (25, 40, 17, 9, "bone"), (45, 40, 30, 9, "acc"),
    (19, 52, 36, 9, "bone"), (58, 52, 20, 9, "acc"),
    (28, 64, 22, 9, "bone"), (54, 64, 26, 9, "acc"),
]
BONE = (0xEC, 0xE6, 0xDC)
ACC = (0x7A, 0x5F, 0xFF)
INK_IN = (0x1A, 0x18, 0x16)   # radial-gradient(120% 120% at 30% 18%, …
INK_OUT = (0x14, 0x12, 0x10)  # … to here

# How much of the canvas the mark spans. The old icon drew it at 0.782 of the SVG's own
# proportions — a 47.7% wide mark in a 100% wide square, which left a border thick enough
# to read as a frame. 1.095 is that times 1.4: the same drawing, 40% bigger (2026-09-06).
MARK_SCALE = 1.095
SS = 4  # supersampling factor per axis


def rounded_rect_coverage(px, py, x, y, w, h, r):
    """1.0 inside the rounded rect, 0.0 outside — sampled, so SS handles the edges."""
    if px < x or px > x + w or py < y or py > y + h:
        return 0.0
    # inside the straight middle bands?
    if x + r <= px <= x + w - r or y + r <= py <= y + h - r:
        return 1.0
    cx = x + r if px < x + r else x + w - r
    cy = y + r if py < y + r else y + h - r
    return 1.0 if (px - cx) ** 2 + (py - cy) ** 2 <= r * r else 0.0


def render(size):
    """Rows of RGB bytes."""
    s = size * SS
    # background: one value per supersampled row/col pair is overkill, so compute per pixel
    # of the FINAL image and let the bars alone carry the supersampling.
    cx, cy = 0.30 * size, 0.18 * size
    rad = 1.20 * size
    unit = size / 100.0 * MARK_SCALE          # one SVG unit in final pixels
    off_x = size / 2 - 50 * unit              # centre the 100x100 box on the canvas
    off_y = size / 2 - 50 * unit

    bars = []
    for bx, by, bw, bh, kind in BARS:
        bars.append((
            off_x + bx * unit, off_y + by * unit, bw * unit, bh * unit,
            bh * unit / 2.0, BONE if kind == "bone" else ACC,
        ))

    rows = []
    inv = 1.0 / (SS * SS)
    for y in range(size):
        row = bytearray(size * 3)
        for x in range(size):
            # ground
            d = (((x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2) ** 0.5) / rad
            t = 1.0 if d > 1 else d
            r0 = int(INK_IN[0] + (INK_OUT[0] - INK_IN[0]) * t + 0.5)
            g0 = int(INK_IN[1] + (INK_OUT[1] - INK_IN[1]) * t + 0.5)
            b0 = int(INK_IN[2] + (INK_OUT[2] - INK_IN[2]) * t + 0.5)
            # bars, supersampled
            for bx, by, bw, bh, br, col in bars:
                if x + 1 < bx or x > bx + bw or y + 1 < by or y > by + bh:
                    continue
                hits = 0
                for sy in range(SS):
                    py = y + (sy + 0.5) / SS
                    for sx in range(SS):
                        px = x + (sx + 0.5) / SS
                        hits += rounded_rect_coverage(px, py, bx, by, bw, bh, br)
                if hits:
                    a = hits * inv
                    r0 = int(r0 + (col[0] - r0) * a + 0.5)
                    g0 = int(g0 + (col[1] - g0) * a + 0.5)
                    b0 = int(b0 + (col[2] - b0) * a + 0.5)
            row[x * 3 : x * 3 + 3] = bytes((r0, g0, b0))
        rows.append(row)
    return rows


def write_png(path, size, rows):
    raw = bytearray()
    for row in rows:
        raw.append(0)
        raw += row
    def chunk(typ, body):
        return struct.pack(">I", len(body)) + typ + body + struct.pack(">I", zlib.crc32(typ + body))
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    open(path, "wb").write(png)


if __name__ == "__main__":
    size = int(sys.argv[1]) if len(sys.argv) > 1 else 1024
    from pathlib import Path
    default = Path(__file__).resolve().parent.parent / "src-tauri/icons/icon.png"
    out = sys.argv[2] if len(sys.argv) > 2 else str(default)
    write_png(out, size, render(size))
    print(f"  wrote {out} ({size}x{size}, mark at {MARK_SCALE:.3f}× the old drawing)")
