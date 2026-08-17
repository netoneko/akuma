#!/usr/bin/env python3
"""Render an HTML slide deck to a 16:9 PDF, one page per slide.

Why screenshots instead of Chrome's --print-to-pdf: Chrome lays print media
out at a ~800px viewport regardless of the @page size and then scales the
result to fill the page. Every `min-width: 60rem` rule in the deck therefore
loses, the two-column slides print as the mobile stack, and the vw-based type
scale is computed at the wrong width and then magnified. Measured with a
breakpoint probe: a 1280x720px @page and a 2276x1280px @page both report a
layout width between 700 and 800px, and --window-size does not move it.

So each slide is captured at exactly 1280x720 in *screen* media — the layout
the deck was designed in — and the PDF is an assembly of those captures. The
trade is that the text is not selectable; the gain is that the PDF cannot
drift from what the browser shows, with no second copy of the type scale to
maintain.

Usage:  scripts/render_deck_pdf.py bootstrap/public/600-bugs/index.html [out.pdf]
"""

import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

SLIDE_W, SLIDE_H = 1280, 720   # CSS px the deck is designed at
SCALE = 2                      # capture at 2x so the page stays sharp zoomed in
PAGE_W, PAGE_H = 960, 540      # PDF page, in pt: 13.333in x 7.5in, standard 16:9
JPEG_QUALITY = 95

# Interaction affordances: nothing in a PDF can be clicked or arrowed through.
HIDE_CHROME = ".rail, .hint { display: none !important; }"


def capture_slides(src: Path, work: Path) -> list[Path]:
    """Screenshot each .slide at SLIDE_W x SLIDE_H, in document order."""
    html = src.read_text()
    count = len(re.findall(r'<section class="slide"', html))
    if not count:
        sys.exit(f"no slides found in {src}")

    shots = []
    for n in range(1, count + 1):
        # <base> lets the temp file live outside the deck dir and still resolve
        # the deck's relative image srcs.
        inject = (
            f'<base href="{src.parent.as_uri()}/">'
            f"<style>{HIDE_CHROME}\n"
            f".deck .slide:not(:nth-of-type({n})) {{ display: none !important; }}"
            f"</style>"
        )
        page = work / f"slide-{n:02d}.html"
        page.write_text(html.replace("</head>", inject + "</head>", 1))

        shot = work / f"slide-{n:02d}.png"
        subprocess.run(
            [CHROME, "--headless", "--disable-gpu", "--hide-scrollbars",
             f"--window-size={SLIDE_W},{SLIDE_H}",
             f"--force-device-scale-factor={SCALE}",
             "--virtual-time-budget=4000",
             f"--screenshot={shot}", str(page)],
            check=True, capture_output=True,
        )
        if not shot.exists():
            sys.exit(f"chrome produced no screenshot for slide {n}")
        shots.append(shot)
    return shots


def to_jpeg(png: Path) -> tuple[bytes, int, int]:
    """PNG -> baseline JPEG bytes + pixel size. DCTDecode wants baseline."""
    jpg = png.with_suffix(".jpg")
    subprocess.run(["sips", "-s", "format", "jpeg",
                    "-s", "formatOptions", str(JPEG_QUALITY),
                    str(png), "--out", str(jpg)],
                   check=True, capture_output=True)
    data = jpg.read_bytes()

    w = h = None
    i = 2
    while i < len(data) - 9:
        if data[i] != 0xFF:
            i += 1
            continue
        marker, seglen = data[i + 1], struct.unpack(">H", data[i + 2:i + 4])[0]
        if marker in (0xC0, 0xC1):                      # SOF0/SOF1: baseline
            h, w = struct.unpack(">HH", data[i + 5:i + 9])
            break
        if marker == 0xC2:
            sys.exit("sips wrote a progressive JPEG; PDF DCTDecode needs baseline")
        i += 2 + seglen
    if not w:
        sys.exit(f"could not read JPEG dimensions from {jpg}")
    return data, w, h


def build_pdf(images: list[tuple[bytes, int, int]], out: Path) -> None:
    """Minimal PDF: one full-bleed image per page. No dependencies."""
    objs: list[bytes] = []          # objs[i] is object number i+1

    def add(body: bytes) -> int:
        objs.append(body)
        return len(objs)

    add(b"")                        # 1: catalog, patched once page ids are known
    add(b"")                        # 2: page tree, same
    page_ids = []
    for data, w, h in images:
        img = add(
            b"<< /Type /XObject /Subtype /Image /Width %d /Height %d "
            b"/ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode "
            b"/Length %d >>\nstream\n" % (w, h, len(data)) + data + b"\nendstream"
        )
        # place the image over the whole page box
        content = b"q %d 0 0 %d 0 0 cm /Im0 Do Q\n" % (PAGE_W, PAGE_H)
        cid = add(b"<< /Length %d >>\nstream\n" % len(content) + content + b"endstream")
        page_ids.append(add(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 %d %d] "
            b"/Resources << /XObject << /Im0 %d 0 R >> >> /Contents %d 0 R >>"
            % (PAGE_W, PAGE_H, img, cid)
        ))

    kids = b" ".join(b"%d 0 R" % p for p in page_ids)
    objs[0] = b"<< /Type /Catalog /Pages 2 0 R >>"
    objs[1] = b"<< /Type /Pages /Count %d /Kids [%s] >>" % (len(page_ids), kids)

    buf = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for n, body in enumerate(objs, start=1):
        offsets.append(len(buf))
        buf += b"%d 0 obj\n" % n + body + b"\nendobj\n"

    xref = len(buf)
    buf += b"xref\n0 %d\n" % (len(objs) + 1)
    buf += b"0000000000 65535 f \n"
    for off in offsets:
        buf += b"%010d 00000 n \n" % off
    buf += (b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n"
            % (len(objs) + 1, xref))
    out.write_bytes(buf)


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    src = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else src.with_suffix(".pdf")
    if not Path(CHROME).exists():
        sys.exit(f"Chrome not found at {CHROME}")

    work = Path(tempfile.mkdtemp(prefix="deckpdf-"))
    try:
        shots = capture_slides(src, work)
        build_pdf([to_jpeg(s) for s in shots], out)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print(f"{len(shots)} slides -> {out} ({out.stat().st_size // 1024} KB)")


if __name__ == "__main__":
    main()
