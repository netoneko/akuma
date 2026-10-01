# Third-party code in `akuma-fbcon`

`akuma-fbcon` is part of Akuma (BSD-2-Clause, see the repository `LICENSE`). It links the
crates below. Each is offered under a permissive licence that is compatible with
BSD-2-Clause and is used here **under its MIT terms** (all four offer MIT); their
copyright notices and permission text are reproduced below, as the MIT licence requires
of any distribution of the code — including a kernel image that contains it. The
vendored fonts keep their own notices in `vendor/*/LICENSE*` (IBM Plex Mono: SIL OFL 1.1;
Spleen: BSD-2-Clause).

Regenerate this file when a version below changes. Licence-compatibility check, 2026-10-01: all four are dual-licensed with MIT; none carries copyleft or an
advertising clause; Apache-2.0 patent terms are not triggered because MIT is chosen.

| crate | version | licence (SPDX) | what it is used for |
|---|---|---|---|
| [`vte`](https://github.com/alacritty/vte) | 0.15.0 | Apache-2.0 OR MIT | Escape-sequence parser for the framebuffer console (UTF-8 decoding, CSI/OSC/ESC states). |
| [`unicode-width`](https://github.com/unicode-rs/unicode-width) | 0.2.2 | MIT OR Apache-2.0 | Display width of a character (0, 1 or 2 columns). |
| [`arrayvec`](https://github.com/bluss/arrayvec) | 0.7.8 | MIT OR Apache-2.0 | Fixed-capacity vector `vte` uses for its OSC buffer (no allocation). |
| [`memchr`](https://github.com/BurntSushi/memchr) | 2.8.3 | Unlicense OR MIT | Byte search `vte` uses to skip plain text. |
| [GNU Unifont](https://unifoundry.com/unifont/) 18.0.01 | OFL-1.1 (dual-licensed with GPLv2+ and the Font Embedding Exception; **used under the OFL**) | The 16x16 CJK/kana/Hangul glyphs and the 8x16 Greek/Cyrillic/Vietnamese fallback (`vendor/unifont/`). Repacked 1-bit; nothing else changed. |
| [Noto Emoji](https://github.com/googlefonts/noto-emoji) images | commit e20cbc2 | Apache-2.0 (image resources); OFL-1.1 (the repository's root `LICENSE`, for the fonts) | The ~650 colour emoji the console draws (`vendor/noto-emoji/`). **Modified**: downscaled to 24x24 and quantised to RGBA4444. |

## vte 0.15.0 — LICENSE-MIT

```text
Copyright (c) 2016 Joe Wilm

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

## unicode-width 0.2.2 — LICENSE-MIT

```text
Copyright (c) 2015 The Rust Project Developers

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

## arrayvec 0.7.8 — LICENSE-MIT

```text
Copyright (c) Ulrik Sverdrup "bluss" 2015-2023

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

## memchr 2.8.3 — LICENSE-MIT

```text
The MIT License (MIT)

Copyright (c) 2015 Andrew Gallant

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## Noto Emoji images (vendored data, not a crate)

`vendor/noto-emoji/` holds a downscaled subset of Google's Noto Emoji PNGs
(`emoji-24.index`, `emoji-24.rgba4444`; regenerate with `scripts/bake_emoji.py`).
Provenance, the exact upstream commit, and the list of emoji requested but absent
upstream are in `vendor/noto-emoji/PROVENANCE.txt`.

**Licence.** The upstream repository is ambiguous: its README says the image
resources are Apache-2.0 and links `./LICENSE`, but the root `LICENSE` is the SIL
Open Font License 1.1 (for the fonts), and the Apache text sits in `2D/svg/LICENSE`.
Both texts are shipped beside the data (`LICENSE-APACHE-2.0.txt`,
`LICENSE-OFL-1.1.txt`) so that either reading is satisfied; both are permissive and
compatible with this project's BSD-2-Clause. As Apache-2.0 section 4 requires, the
files carry this notice that they were **changed** (resized from 128x128 to 24x24 and
quantised), and the upstream notice is kept:

```text
Copyright 2013 Google, Inc. All Rights Reserved.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
```

(`vendor/noto-emoji/NOTICE-APACHE-HEADER.txt` is the upstream header verbatim.)

## GNU Unifont (vendored data, not a crate)

`vendor/unifont/wide-16.bin` and `narrow-8.bin` are repacked glyphs from GNU Unifont
18.0.01 (`scripts/bake_unifont.py`; provenance in `vendor/unifont/PROVENANCE.txt`).
Unifont's own copyright line reads: "Copyright (C) 1998-2026 Roman Czyborra, Paul Hardy,
Qianqian Fang, Andrew Miller, Johnnie Weaver, David Corbett, Ælla Chiana Moskopp,
Rebecca Bettencourt, Minseo Lee, Ho-Seok Ee, et al. License: SIL Open Font License
version 1.1 and GPLv2+: GNU GPL version 2 or later with the GNU Font Embedding
Exception." This project relies on the **OFL 1.1** alternative only; its text (with that
copyright line) is `vendor/unifont/LICENSE-OFL-1.1.txt`. The OFL's conditions — keep the
notice, do not sell the font by itself, do not use a Reserved Font Name for a modified
version — are met: the data is embedded in a kernel, with the notice, and is not
distributed as a font.
