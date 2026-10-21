#!/usr/bin/env python3
"""Generate the Open Download Manager icon: a rounded square with a ripple/wave mark.

Writes a PNG (for egui/eframe and the Linux .desktop file) and an ICO
(for the Windows executable, via the `winresource` crate's format).

No image library is available here, so the PNG is written by hand: a zlib
stream of raw scanlines, which is all a PNG really is.
"""
import math
import struct
import zlib
import sys
import os

OUT = sys.argv[1] if len(sys.argv) > 1 else "."

# Palette: a deep blue field with a lighter wave, matching the app's dark theme.
BG_TOP = (37, 99, 235)
BG_BOTTOM = (29, 78, 216)
WAVE = (240, 246, 255)
WAVE2 = (147, 197, 253)


def render(size):
    """Return an RGBA pixel buffer of the icon at `size`."""
    px = [[(0, 0, 0, 0) for _ in range(size)] for _ in range(size)]
    r = size * 0.22  # corner radius of the rounded square
    for y in range(size):
        for x in range(size):
            # Rounded-rectangle coverage, with a little anti-aliasing at the edge.
            cx = min(max(x + 0.5, r), size - r)
            cy = min(max(y + 0.5, r), size - r)
            d = math.hypot(x + 0.5 - cx, y + 0.5 - cy)
            if d > r:
                continue
            edge = min(1.0, max(0.0, r - d + 0.5))
            t = y / max(1, size - 1)
            base = tuple(
                int(round(BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t)) for i in range(3)
            )
            px[y][x] = (base[0], base[1], base[2], int(round(255 * edge)))

    # Three ripple strokes, like a wave through water.
    for k, (amp, phase, thick, colour) in enumerate(
        [
            (0.16, 0.0, 0.052, WAVE),
            (0.115, 1.1, 0.042, WAVE2),
            (0.075, 2.2, 0.032, WAVE2),
        ]
    ):
        y_mid = size * (0.5 + 0.10 * k)
        for x in range(size):
            f = x / max(1, size - 1)
            wave = math.sin(f * math.pi * 2.0 + phase)
            yc = y_mid + wave * size * amp
            half = size * thick / 2.0
            y0, y1 = int(math.floor(yc - half)), int(math.ceil(yc + half))
            for y in range(max(0, y0), min(size, y1 + 1)):
                dist = abs(y + 0.5 - yc)
                if dist > half:
                    continue
                cover = min(1.0, max(0.0, (half - dist) + 0.5))
                r0, g0, b0, a0 = px[y][x]
                if a0 == 0:
                    continue
                px[y][x] = (
                    int(round(r0 + (colour[0] - r0) * cover)),
                    int(round(g0 + (colour[1] - g0) * cover)),
                    int(round(b0 + (colour[2] - b0) * cover)),
                    a0,
                )
    return px


def write_png(path, px):
    size = len(px)
    raw = bytearray()
    for row in px:
        raw.append(0)  # filter type 0
        for r, g, b, a in row:
            raw += bytes((r, g, b, a))

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def write_ico(path, sizes):
    """A PNG-in-ICO (Vista+ understands PNG entries)."""
    entries = []
    blobs = []
    offset = 6 + 16 * len(sizes)
    for s in sizes:
        px = render(s)
        png_path = f"{path}.{s}.png"
        write_png(png_path, px)
        with open(png_path, "rb") as f:
            data = f.read()
        os.remove(png_path)
        entries.append(struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset))
        blobs.append(data)
        offset += len(data)
    out = struct.pack("<HHH", 0, 1, len(sizes)) + b"".join(entries) + b"".join(blobs)
    with open(path, "wb") as f:
        f.write(out)


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    write_png(os.path.join(OUT, "odm.png"), render(512))
    write_png(os.path.join(OUT, "odm-256.png"), render(256))
    write_ico(os.path.join(OUT, "odm.ico"), [16, 32, 48, 64, 128, 256])
    print("icons written to", OUT)
