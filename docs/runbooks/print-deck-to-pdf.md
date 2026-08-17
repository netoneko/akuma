# Print a slide deck to PDF

Use this when you need a PDF of an HTML deck under `bootstrap/public/` — to
hand out, to project from a machine without the repo, or to attach to a talk
submission. Currently two decks live there: `600-bugs/index.html` and
`deck/index.html`.

**Do not use Cmd-P or `chrome --print-to-pdf` for these decks.** Both produce a
deck that stacks into its mobile layout with the type scale magnified. The
reason is measured, not suspected: Chrome lays print media out at a **~800px
viewport regardless of the `@page` size**, then scales the result to fill the
page — so every `min-width: 60rem` two-column rule loses, and every
`clamp(…, Nvw, …)` font size is computed against the wrong width and then blown
up. See **Why the obvious way fails** below for the probe if you want to
re-confirm it.

Use the script instead. It captures each slide in *screen* media at exactly
1280x720 — the layout the decks are designed at — and assembles the captures
into a 16:9 PDF.

## 1. Render

```bash
python3 scripts/render_deck_pdf.py bootstrap/public/600-bugs/index.html \
                                   bootstrap/public/600-bugs/600-bugs.pdf
```

The second argument is optional; it defaults to the input path with a `.pdf`
suffix. Expect ~30 seconds for 15 slides (one Chrome launch per slide).

```
15 slides -> /…/bootstrap/public/600-bugs/600-bugs.pdf (4780 KB)
```

Requirements are all preinstalled on macOS: Chrome at
`/Applications/Google Chrome.app`, `python3` (no third-party packages), and
`sips` for the PNG->JPEG step. The script finds slides by counting
`<section class="slide">`, hides `.rail` and `.hint` (interaction affordances —
nothing in a PDF can be clicked or arrowed through), and injects a `<base>` so
each slide's relative `<img src>` still resolves from the temp file.

## 2. Verify

Page count, page size, and one image per page:

```bash
python3 - <<'PY'
import re
d = open('bootstrap/public/600-bugs/600-bugs.pdf','rb').read()
print('pages   ', len(re.findall(rb'/Type /Page[^s]', d)))
print('boxes   ', set(re.findall(rb'/MediaBox\s*\[[^\]]*\]', d)))
print('images  ', len(re.findall(rb'/Subtype /Image', d)))
PY
```

Expect the page count to equal the number of slides, one distinct MediaBox, and
one image per page:

```
pages    15
boxes    {b'/MediaBox [0 0 960 540]'}
images   15
```

`960x540` pt is 13.333in x 7.5in — the standard 16:9 slide page.

**Then look at the pages.** The counts above cannot see a clipped table or an
overflowing slide. There is no `pdftoppm` / `pypdf` / ImageMagick on this
machine, and `sips` rasterizes **page 1 only**:

```bash
sips -s format png bootstrap/public/600-bugs/600-bugs.pdf --out /tmp/p1.png
```

To eyeball every page, re-run the capture step on its own — those PNGs *are*
the PDF's pages, at 2x:

```bash
python3 - <<'PY'
import importlib.util
from pathlib import Path
spec = importlib.util.spec_from_file_location('r', 'scripts/render_deck_pdf.py')
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
out = Path('/tmp/deck-shots'); out.mkdir(exist_ok=True)
print(len(m.capture_slides(Path('bootstrap/public/600-bugs/index.html').resolve(), out)))
PY
open /tmp/deck-shots            # slide-01.png … slide-NN.png, 2560x1440
```

Check the dense slides specifically — tables, `.figs` rows, and any slide with
both a table and an image are the ones that overflow 720px first.

## 3. Troubleshooting

| Symptom | Cause |
|---|---|
| `no slides found` | The deck does not use `<section class="slide">`. Adjust the regex in `capture_slides`, do not switch to `--print-to-pdf` |
| Slides stacked into one column, huge type | You used Cmd-P or `--print-to-pdf`. See above |
| Nav rail or `↑ ↓` hint visible in the PDF | The deck renamed `.rail` / `.hint`; update `HIDE_CHROME` in the script |
| Missing images, alt text showing | The `<base>` injection failed, or the `<img src>` is absolute to a host that is not reachable offline |
| A slide is blank or half-painted | Raise `--virtual-time-budget` (currently 4000 ms) — web fonts or a large JPEG had not decoded |
| `sips wrote a progressive JPEG` | PDF `DCTDecode` needs baseline. The script refuses rather than emitting a PDF that some viewers will not open |
| PDF is much bigger than you want | Lower `JPEG_QUALITY` (95) or `SCALE` (2) in the script. At 2x/95 a 15-slide deck is ~4.8 MB |

**The text is not selectable** — the pages are images. That is the deliberate
trade: the alternative is a second copy of the deck's type scale written in
absolute px for print, which drifts from the screen design the first time
anyone edits either one.

## Why the obvious way fails

Kept because it is cheap to re-derive wrongly. Inject a breakpoint probe into
the deck and print *that*: a print-only element whose `::after` content is
rewritten by a nested `@media (min-width: Npx)` for each N you care about. The
content that survives names the print layout width.

Measured 2026-08-17, Chrome 16:9 pages, `@page` inside `@media print`:

| `@page size` | resulting `MediaBox` | print layout width |
|---|---|---|
| `13.333in 7.5in` | 960x540 pt | **700–800px** |
| `1280px 720px` | 960x540 pt | **700–800px** |
| `2276px 1280px` | 1706x960 pt | **700–800px** |

`--window-size=1280,720`, `--force-device-scale-factor=1` and
`--headless=new` change none of it. The page box scales the *output*; it does
not set the layout viewport.

The decks keep a `@media print` block anyway (`600-bugs/index.html`) — it hides
the rail, paginates one slide per page, and forces `print-color-adjust: exact`
so the dark ground survives, which makes an accidental Cmd-P legible rather
than faithful. `deck/print.html` is an older answer to the same problem: a
whole second copy of that deck with the print rules inlined. Do not add a third
copy; extend the script.

## Background

- [`../../scripts/render_deck_pdf.py`](../../scripts/render_deck_pdf.py) — the
  renderer, including the ~100-line dependency-free PDF writer (one baseline
  JPEG per page as a `DCTDecode` XObject). Its module docstring carries the same
  measurement as the table above.
- `bootstrap/public/600-bugs/index.html` § "print / PDF" — the `@media print`
  block and why it cannot match the screen.
- No `../archive/` original: this runbook is the first record of the
  print-viewport finding, which came out of exporting the 600-bugs deck on
  2026-08-17.
