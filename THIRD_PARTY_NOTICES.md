# Third-party notices

Akuma is BSD-2-Clause (see `LICENSE`). This file reproduces the notices that
code derived from, or built into, Akuma requires. Files that are not listed
here are original. Fonts and the `akuma-fbcon` dependencies have their own file:
`crates/akuma-fbcon/THIRD_PARTY_LICENSES.md`.

## Derived from other projects

| Where | Derived from | Licence | Notice |
|---|---|---|---|
| `crates/akuma-rtw89` | Linux `drivers/net/wireless/realtek/rtw89/` (v6.17) | GPL-2.0 OR BSD-3-Clause, used as BSD-3-Clause | In `crates/akuma-rtw89/src/lib.rs` |
| `userspace/rumpkernel/rumpuser/src/fiber.rs` | NetBSD `lib/librumpuser/rumpfiber.c`, itself partly from Xen MiniOS `sched.c` | BSD-2-Clause, and MIT (Xen MiniOS) | Below |
| `userspace/rumpkernel/rumpuser/virtif_user_instr.c` | NetBSD `sys/rump/net/lib/libvirtif/virtif_user.c` (instrumentation added) | BSD-2-Clause | Below |
| `userspace/rumpkernel/rumpuser/sp_serve_fd.c` | NetBSD `lib/librumpuser/rumpuser_sp.c` (`#include`d unmodified; the serve loop is reduced from `spserver`) | BSD-2-Clause | Below |
| `overlays/{ryzen,devbox}/rootfs/etc/profile` | The **Tokyo Night** colour palette: values from `folke/tokyonight.nvim` (Folke Lemaitre), a port of the `tokyo-night-vscode-theme` (Enkia) | Apache-2.0 (tokyonight.nvim); MIT (Tokyo Night VS Code theme) | Below, and `third_party/tokyo-night/` |

The NetBSD rump kernel sources themselves are the `userspace/rumpkernel/src-netbsd`
submodule and keep their own per-file notices.

### Tokyo Night colour palette (Folke Lemaitre; Enkia)

The shell theme in `overlays/ryzen/rootfs/etc/profile` and
`overlays/devbox/rootfs/etc/profile` uses the colour values of the **Tokyo Night**
palette, "night" variant, as given by
[tokyonight.nvim](https://github.com/folke/tokyonight.nvim)
(`lua/tokyonight/colors/night.lua`, over `storm.lua`; checked against commit
`cdc07ac78467a233fd62c493de29a17e0cf2b2b6`).

tokyonight.nvim is Copyright its authors (Folke Lemaitre, `folke`, and contributors),
licensed under the **Apache License, Version 2.0**; the full text is in
`third_party/tokyo-night/LICENSE-tokyonight.nvim`, and the licence may also be
obtained at <http://www.apache.org/licenses/LICENSE-2.0>. Upstream names no copyright
holder in its `LICENSE` and ships no `NOTICE` file.

tokyonight.nvim is a port of the **Tokyo Night** theme for Visual Studio Code by
Enkia, <https://github.com/tokyo-night/tokyo-night-vscode-theme>:

```text
The MIT License (MIT)

Copyright (c) 2018-present Enkia

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

What was taken: fourteen colour values, copied unchanged into `export TN_*` lines
(the other palette entries are unused). What was not: the `TN_*` names, the xterm-256
approximations, the `LS_COLORS` / `GREP_COLORS` mappings and the prompt are written
for Akuma; `TN_BLACK` (`#15161e`) is not defined in upstream's `night.lua` or
`storm.lua`. No upstream file is included or modified (Apache-2.0 section 4(b)).
Details: `third_party/tokyo-night/README.md`.

### NetBSD `rumpfiber.c` (Antti Kantee, Justin Cormack)

```text
Copyright (c) 2007-2013 Antti Kantee.  All Rights Reserved.
Copyright (c) 2014 Justin Cormack.  All Rights Reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:
1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS
OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
SUCH DAMAGE.
```

### NetBSD `virtif_user.c` (Antti Kantee)

```text
Copyright (c) 2013 Antti Kantee.  All Rights Reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:
1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS
OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
SUCH DAMAGE.
```

### NetBSD `rumpuser_sp.c` (Antti Kantee)

```text
Copyright (c) 2010, 2011 Antti Kantee.  All Rights Reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:
1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS
OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
SUCH DAMAGE.
```

### Xen MiniOS `sched.c` (via `rumpfiber.c`)

```text
(C) 2005 - Grzegorz Milos - Intel Research Cambridge

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to
deal in the Software without restriction, including without limitation the
rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
sell copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

## Written from observation, not copied

The Realtek RTL8169 driver (`crates/akuma-net-rtl8169`), xHCI, EHCI, NVMe, HDA
and EC code were written against hardware and public specifications; no driver
source was copied. `crates/akuma-ieee80211` and `crates/akuma-wpa` implement the
standards and are tested against RFC/FIPS vectors and frames captured from Linux.

## Rust dependencies compiled into Akuma binaries

Notices for these are in each crate's published source (licence files) and
should be reproduced if binaries are redistributed.

- BSD-3-Clause: `curve25519-dalek`, `ed25519-dalek`, `x25519-dalek`, `subtle`, `sha1_smol`
- MPL-2.0 (unmodified): `fdt`
- `arm_pl031`: offered as GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0; used under Apache-2.0
- 0BSD: `smoltcp`, `managed`
- MIT: `talc`, `virtio-drivers`, `libm`, `generic-array`
- Apache-2.0: `embedded-tls` (fork, pinned in `userspace/Cargo.toml`; modified
  from upstream), `picojson`, `ab_glyph`, `ab_glyph_rasterizer`, `owned_ttf_parser`
- All other dependencies are MIT, Apache-2.0, or dual-licensed with them.

Regenerate the list with `cargo metadata` in the repository root and in
`userspace/`.

## Fetched at build time, not in this repository

busybox, `apk-tools-static` (both GPL-2.0), CA certificates (MPL-2.0) and other
Alpine packages are downloaded by `scripts/populate_disk.sh`,
`userspace/apk-tools/build.rs` and `overlays/devbox/bootstrap.sh`. `tcc`
(LGPL-2.1) is the `userspace/tcc/tinycc` submodule. They keep their own licences.
