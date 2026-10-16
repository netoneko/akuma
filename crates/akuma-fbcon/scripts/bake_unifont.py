#!/usr/bin/env python3
"""Bake GNU Unifont's glyphs into two small binaries: wide (two-column) and narrow.

One-off, developer-run; the OUTPUT is committed (`vendor/unifont/wide-16.bin`), so
no build needs this or the network.

    curl -O https://ftp.gnu.org/gnu/unifont/unifont-18.0.01/unifont-18.0.01.bdf.gz
    gunzip unifont-18.0.01.bdf.gz
    python3 scripts/bake_unifont.py unifont-18.0.01.bdf

`wide-16.bin` — format: for each code point in WIDE, in order, 32 bytes — 16 rows of 16 pixels,
2 bytes a row, most significant bit leftmost, 1 = ink. A code point Unifont has no
glyph for is 32 zero bytes (the console treats an empty glyph, other than U+3000
IDEOGRAPHIC SPACE, as missing). `RANGES` must equal `src/cjk.rs`'s; its test checks
the file length against it.

Unifont is dual-licensed: SIL OFL 1.1, or GPLv2+ with the GNU Font Embedding
Exception. This project uses it under the OFL (`vendor/unifont/LICENSE-OFL-1.1.txt`).
"""

import os
import sys

# `narrow-8.bin`: for each code point in NARROW, 16 bytes (16 rows of 8 pixels, one byte
# a row, most significant bit leftmost) — the scripts the text fonts lack (Greek, most of
# Cyrillic, Latin Extended-B, IPA, Vietnamese). `src/unifont.rs` has the same lists.
#
# Symbols (arrows, math operators, technical, geometric shapes, dingbats...) are baked
# twice, because Unifont draws some of them 8 pixels wide and some 16: `narrow-8.bin`
# takes the 8-wide ones, `symbol-16.bin` the 16-wide ones (Claude Code's `⏺ ✻ ✽ ✶ ✳ ✢ ⏵ ➜`
# are all 16 wide). A code point is empty in the table that does not have it.
SYMBOLS = [
    (0x2040, 0x2070),  # general punctuation: low asterisk, reversed pilcrow...
    (0x2100, 0x218F),  # letterlike symbols, number forms
    (0x2190, 0x21FF),  # arrows
    (0x2200, 0x23FF),  # mathematical operators, miscellaneous technical (⎿ ⏺ ⏵ ⌘)
    (0x25A0, 0x25FF),  # geometric shapes (◐ ◑ ◒ ◓)
    (0x2600, 0x27BF),  # miscellaneous symbols, dingbats (✻ ✽ ❯ ➜ ⚠)
    (0x27F0, 0x27FF),  # supplemental arrows-A
    (0x2900, 0x297F),  # supplemental arrows-B
    (0x2B00, 0x2BFF),  # miscellaneous symbols and arrows
]

NARROW = [
    (0x0180, 0x02AF),  # Latin Extended-B, IPA extensions
    (0x0370, 0x03FF),  # Greek and Coptic
    (0x0400, 0x052F),  # Cyrillic and Cyrillic Supplement
    (0x1E00, 0x1EFF),  # Latin Extended Additional (Vietnamese)
] + SYMBOLS

WIDE = RANGES = [
    (0x3000, 0x30FF),  # CJK symbols and punctuation, Hiragana, Katakana
    (0x4E00, 0x9FFF),  # CJK Unified Ideographs
    (0xAC00, 0xD7A3),  # Hangul syllables
    (0xFF01, 0xFF60),  # fullwidth forms
]
OUT = os.path.join(os.path.dirname(__file__), "..", "vendor", "unifont")


def parse(path):
    glyphs, enc, w, rows, in_bitmap = {}, None, 0, [], False
    for line in open(path, encoding="utf-8"):
        line = line.strip()
        if line.startswith("ENCODING "):
            enc = int(line.split()[1])
        elif line.startswith("BBX "):
            w = int(line.split()[1])
        elif line == "BITMAP":
            in_bitmap, rows = True, []
        elif line == "ENDCHAR":
            in_bitmap = False
            if enc is not None and w in (8, 16) and len(rows) == 16:
                glyphs[(enc, w)] = rows
            enc = None
        elif in_bitmap and line:
            rows.append(int(line, 16))
    return glyphs


def pack(glyphs, ranges, width):
    out, missing = bytearray(), 0
    for a, b in ranges:
        for cp in range(a, b + 1):
            rows = glyphs.get((cp, width))
            if rows is None:
                missing += 1
                out += bytes(16 * (width // 8))
            else:
                for r in rows:
                    out += r.to_bytes(width // 8, "big")
    return bytes(out), missing


def main():
    glyphs = parse(sys.argv[1])
    os.makedirs(OUT, exist_ok=True)
    for name, ranges, width in (
        ("wide-16.bin", WIDE, 16),
        ("narrow-8.bin", NARROW, 8),
        ("symbol-16.bin", SYMBOLS, 16),
    ):
        data, missing = pack(glyphs, ranges, width)
        with open(os.path.join(OUT, name), "wb") as f:
            f.write(data)
        n = sum(b - a + 1 for a, b in ranges)
        print(f"{name}: {n} code points, {len(data)} bytes, {missing} without a {width}x16 glyph")


if __name__ == "__main__":
    main()
