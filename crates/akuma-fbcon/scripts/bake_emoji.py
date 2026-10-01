#!/usr/bin/env python3
"""Bake the console's emoji set from Noto Emoji into two small binary files.

One-off, developer-run (needs Pillow and the network); the OUTPUT is committed, so
no build ever needs either. Writes into `vendor/noto-emoji/`:

  emoji-24.index      N little-endian u32 code points, sorted ascending
  emoji-24.rgba4444   N images of 24x24, 16 bits per pixel, row-major, little-endian:
                      bits 15..12 R, 11..8 G, 7..4 B, 3..0 A (straight, not premultiplied)
  PROVENANCE.txt      what was fetched, from where, at which commit

Source: https://github.com/googlefonts/noto-emoji `2D/png/128/emoji_uXXXX.png`
(images: Apache License 2.0, copyright Google LLC). Downscaled with Lanczos after
premultiplying alpha, so a transparent edge does not bleed its hidden colour.

    python3 -m venv /tmp/ev && /tmp/ev/bin/pip install pillow
    /tmp/ev/bin/python scripts/bake_emoji.py

Adding an emoji is: add its code point to WANT below, run this, commit the output.
"""

import io
import os
import struct
import sys
import urllib.request

from PIL import Image

COMMIT = "e20cbc2bbec1926686be9f9bee7d1d2cfa1fea0e"
BASE = f"https://raw.githubusercontent.com/googlefonts/noto-emoji/{COMMIT}/2D/png/128/emoji_u{{:x}}.png"
SIZE = 24
OUT = os.path.join(os.path.dirname(__file__), "..", "vendor", "noto-emoji")


def span(a, b):
    return list(range(a, b + 1))


# What a chat client and a shell prompt actually print. Ranges are Unicode blocks'
# emoji; anything Noto does not have is skipped and reported.
WANT = sorted(set(
    span(0x1F600, 0x1F64F)      # emoticons
    + span(0x1F440, 0x1F450)    # eyes, ears, hands: 👀 👂 👆 👇 👈 👉 👊 👋 👌 👍 👎 👏 👐
    + [0x1F4AA, 0x1F64C, 0x1F64F, 0x1F485, 0x1F5A4]
    + span(0x1F910, 0x1F93A)    # 🤐 .. 🤺: thinking, shrug, hands, faces
    + span(0x1F970, 0x1F97A)
    + [0x1F90D, 0x1F90E, 0x1F9E0]
    + span(0x1F48B, 0x1F49F)    # hearts and kisses
    + span(0x1F4A0, 0x1F4AF)    # 💠 .. 💯
    + [0x2764, 0x2763]
    + span(0x1F300, 0x1F320)    # weather, moon, rainbow, earth
    + span(0x1F330, 0x1F37F)    # plants and food
    + span(0x1F380, 0x1F3CF)    # party, games, sport
    + [0x1F3C6, 0x1F947, 0x1F948, 0x1F949, 0x1F396, 0x1F3C5]
    + span(0x1F400, 0x1F43F)    # animals
    + span(0x1F980, 0x1F9A2)
    + span(0x1F4BB, 0x1F4BF)    # computers, disks
    + span(0x1F4C1, 0x1F4D0)    # folders, documents, charts, pins
    + span(0x1F4D1, 0x1F4E3)    # books, notes, megaphones
    + span(0x1F4E4, 0x1F4F9)    # mail, phones, cameras
    + span(0x1F50D, 0x1F52E)    # search, locks, keys, bell, link, fire, tools
    + [0x1F680, 0x1F6A8, 0x1F6AB, 0x1F6E0, 0x1F6D1]
    + [0x1F534, 0x1F535] + span(0x1F7E0, 0x1F7EB)  # coloured circles and squares
    + span(0x1F536, 0x1F53D)
    + [0x2705, 0x274C, 0x274E, 0x2753, 0x2754, 0x2755, 0x2757, 0x2B50, 0x2B1B, 0x2B1C,
       0x2B55, 0x2728, 0x26A1, 0x26A0, 0x203C, 0x2049, 0x26D4, 0x2716, 0x2795, 0x2796,
       0x2797, 0x27B0, 0x27BF, 0x26AA, 0x26AB, 0x267B, 0x2699, 0x2694, 0x270C, 0x263A,
       0x2639, 0x2600, 0x2601, 0x2614, 0x2615, 0x26C4, 0x26C5, 0x2744, 0x231A, 0x231B,
       0x23E9, 0x23EA, 0x23EB, 0x23EC, 0x23F0, 0x23F3, 0x2B06, 0x2B07, 0x27A1, 0x2B05]
))


def fetch(cp):
    try:
        with urllib.request.urlopen(BASE.format(cp), timeout=30) as r:
            return r.read()
    except Exception:
        return None


def to_4444(img):
    """Premultiplied Lanczos downscale, then straight RGBA4444."""
    img = img.convert("RGBA")
    px = img.load()
    w, h = img.size
    pre = Image.new("RGBA", (w, h))
    pp = pre.load()
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            pp[x, y] = (r * a // 255, g * a // 255, b * a // 255, a)
    small = pre.resize((SIZE, SIZE), Image.LANCZOS)
    sp = small.load()
    out = bytearray()
    for y in range(SIZE):
        for x in range(SIZE):
            r, g, b, a = sp[x, y]
            if a:  # un-premultiply
                r, g, b = min(255, r * 255 // a), min(255, g * 255 // a), min(255, b * 255 // a)
            q = lambda v: (v * 15 + 127) // 255
            out += struct.pack("<H", (q(r) << 12) | (q(g) << 8) | (q(b) << 4) | q(a))
    return bytes(out)


def main():
    got, missing, blobs = [], [], []
    for cp in WANT:
        data = fetch(cp)
        if data is None:
            missing.append(cp)
            continue
        blobs.append(to_4444(Image.open(io.BytesIO(data))))
        got.append(cp)
        sys.stderr.write(f"\r{len(got)}/{len(WANT)}")
    sys.stderr.write("\n")
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(OUT, "emoji-24.index"), "wb") as f:
        f.write(b"".join(struct.pack("<I", cp) for cp in got))
    with open(os.path.join(OUT, "emoji-24.rgba4444"), "wb") as f:
        f.write(b"".join(blobs))
    with open(os.path.join(OUT, "PROVENANCE.txt"), "w") as f:
        f.write(
            "Noto Emoji images, downscaled by scripts/bake_emoji.py (MODIFIED: resized from\n"
            "128x128 to 24x24 and quantised to RGBA4444).\n\n"
            f"Source:  https://github.com/googlefonts/noto-emoji  commit {COMMIT}\n"
            "Path:    2D/png/128/emoji_uXXXX.png\n"
            "Licence: Apache License 2.0 (see LICENSE), (c) Google LLC\n"
            f"Count:   {len(got)} emoji; {len(missing)} requested but absent upstream: "
            + ", ".join(f"U+{c:04X}" for c in missing) + "\n"
        )
    print(f"baked {len(got)} emoji ({len(got) * SIZE * SIZE * 2} bytes), {len(missing)} missing")


if __name__ == "__main__":
    main()
