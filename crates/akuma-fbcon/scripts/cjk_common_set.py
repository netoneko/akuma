#!/usr/bin/env python3
"""Print the common CJK set `tests/bake_hires.rs` bakes, one hex code point a line.

GB2312 (Simplified Chinese) U JIS X 0208 (Japanese) U KS X 1001 Hangul (Korean), plus all of
kana / CJK punctuation (U+3000-30FF) and the fullwidth forms (U+FF01-FF60). About 12.5k
glyphs, the ones a reader meets: every other character stays on Unifont's 1-bit bitmap.
Only the standard library's codecs are used, so there is nothing to install.
"""

def decode_all(codec, leads, trails):
    out = set()
    for a in leads:
        for b in trails:
            try:
                c = bytes([a, b]).decode(codec)
            except UnicodeDecodeError:
                continue
            if len(c) == 1 and ord(c) >= 0x3000:
                out.add(ord(c))
    return out

gb = decode_all("gb2312", range(0xA1, 0xF8), range(0xA1, 0xFF))
jis = decode_all("euc_jp", range(0xA1, 0xF5), range(0xA1, 0xFF))
ks = decode_all("euc_kr", range(0xA1, 0xFE), range(0xA1, 0xFF))
hangul = {c for c in ks if 0xAC00 <= c <= 0xD7A3}
fixed = set(range(0x3000, 0x3100)) | set(range(0xFF01, 0xFF61))
for cp in sorted(gb | jis | hangul | fixed):
    # Only the BMP ranges the console's wide path knows (unifont::WIDE).
    if 0x3000 <= cp <= 0x30FF or 0x4E00 <= cp <= 0x9FFF or 0xAC00 <= cp <= 0xD7A3 or 0xFF01 <= cp <= 0xFF60:
        print(f"{cp:04X}")
