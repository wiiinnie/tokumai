#!/usr/bin/env python3
"""Make the iOS app icons opaque — App Store icons may not carry an alpha channel.

TestFlight and the App Store drop an icon that has transparency and show a grey
placeholder instead; the upload itself succeeds, so nothing tells you (2026-09-05, the
first tokumai build). iOS masks the icon with its own superellipse anyway, so the rounded
corners in the source are not only unnecessary — they are what makes it invalid.

Flattens every PNG in gen/apple/Assets.xcassets/AppIcon.appiconset over the icon's own
background colour and rewrites it without alpha. Idempotent: a file that is already opaque
is left untouched. Pure standard library (no PIL on this machine), 8-bit RGBA, the only
shape tauri produces.

    scripts/ios-icons-opaque.py [--check]

--check exits 1 if anything still has an alpha channel and changes nothing — that is how
the TestFlight script uses it.
"""

import struct
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "src-tauri/gen/apple/Assets.xcassets/AppIcon.appiconset"
# The design's ink. Only the fully transparent corners end up this colour; everything else
# is composited over it and keeps its own value.
BG = (0x14, 0x12, 0x10)


def chunks(data):
    pos = 8
    while pos < len(data):
        (ln,) = struct.unpack(">I", data[pos : pos + 4])
        yield data[pos + 4 : pos + 8], data[pos + 8 : pos + 8 + ln]
        pos += 12 + ln


def read_rgba(data):
    """(width, height, rows) for an 8-bit RGBA non-interlaced PNG, or None."""
    idat = b""
    w = h = None
    for typ, body in chunks(data):
        if typ == b"IHDR":
            w, h, depth, ctype, comp, filt, interlace = struct.unpack(">IIBBBBB", body)
            if (depth, ctype, interlace) != (8, 6, 0):
                return None
        elif typ == b"IDAT":
            idat += body
    if w is None:
        return None
    raw = zlib.decompress(idat)
    stride = w * 4
    out, prev = [], bytearray(stride)
    pos = 0
    for _ in range(h):
        ft = raw[pos]
        line = bytearray(raw[pos + 1 : pos + 1 + stride])
        pos += 1 + stride
        for i in range(stride):
            a = line[i - 4] if i >= 4 else 0
            b = prev[i]
            c = prev[i - 4] if i >= 4 else 0
            if ft == 1:
                line[i] = (line[i] + a) & 0xFF
            elif ft == 2:
                line[i] = (line[i] + b) & 0xFF
            elif ft == 3:
                line[i] = (line[i] + (a + b) // 2) & 0xFF
            elif ft == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        out.append(line)
        prev = line
    return w, h, out


def write_rgb(path, w, h, rows):
    raw = bytearray()
    for row in rows:
        raw.append(0)  # filter: none — these are small files, compression is plenty
        raw += row
    def chunk(typ, body):
        return struct.pack(">I", len(body)) + typ + body + struct.pack(">I", zlib.crc32(typ + body))
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


def main():
    check = "--check" in sys.argv
    if not ICONS.is_dir():
        sys.exit(f"no icon set at {ICONS}")
    with_alpha = []
    for png in sorted(ICONS.glob("*.png")):
        data = png.read_bytes()
        parsed = read_rgba(data)
        if parsed is None:
            continue  # already RGB, or a shape this script does not touch
        with_alpha.append(png)
        if check:
            continue
        w, h, rows = parsed
        flat = []
        for row in rows:
            out = bytearray(w * 3)
            for x in range(w):
                r, g, b, a = row[x * 4 : x * 4 + 4]
                if a == 255:
                    out[x * 3 : x * 3 + 3] = bytes((r, g, b))
                else:
                    out[x * 3 : x * 3 + 3] = bytes(
                        ((v * a + bg * (255 - a) + 127) // 255 for v, bg in zip((r, g, b), BG))
                    )
            flat.append(out)
        write_rgb(png, w, h, flat)
        print(f"  flattened {png.name} ({w}x{h})")

    if check and with_alpha:
        print("iOS icons still carry an alpha channel — TestFlight will show a placeholder:")
        for p in with_alpha:
            print(f"    {p.name}")
        print("  fix: scripts/ios-icons-opaque.py")
        sys.exit(1)
    if check:
        print("  icons are opaque")
    elif not with_alpha:
        print("  nothing to do — already opaque")


if __name__ == "__main__":
    main()
