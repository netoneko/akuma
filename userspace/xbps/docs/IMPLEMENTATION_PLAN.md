# XBPS for Akuma OS — Implementation

## Overview

XBPS (X Binary Package System, the Void Linux package manager) is cross-compiled
and packaged for Akuma OS as 16 statically-linked `aarch64-linux-musl` binaries.

**Source:** https://github.com/void-linux/xbps (tag `0.60.7`)
**Location in repo:** `userspace/xbps/xbps/` (git submodule)
**Build entry point:** `userspace/xbps/build.rs`
**Output:** `userspace/xbps/dist/xbps.tar` → `bootstrap/archives/xbps.tar`

---

## Architecture

The `xbps` Cargo package follows the **Lib Trick** pattern:
- `src/lib.rs` — `#![no_std]` (prevents Cargo from building a competing binary)
- `build.rs` — downloads deps, cross-compiles everything, creates tar

```
userspace/xbps/
├── Cargo.toml           Library crate (Lib Trick)
├── build.rs             All build logic (~400 lines)
├── src/lib.rs           #![no_std] placeholder
├── xbps/                Git submodule: void-linux/xbps
├── vendor/              Downloaded dependency sources (gitignored)
├── build/deps/          Installed static libs + headers (gitignored)
└── dist/xbps.tar        Final archive (56 MB, 16 binaries)
```

---

## Dependency Chain

Six components built in strict order:

| # | Library | Version | Purpose | Build method |
|---|---------|---------|---------|-------------|
| 1 | zlib | 1.3.1 | Compression | configure + make (Makefile patched for cross-AR) |
| 2 | lz4 | 1.9.4 | Fast compression | make -C lib (static only, manual install) |
| 3 | zstd | 1.5.5 | Modern compression | make install-static, install-pc, install-includes |
| 4 | LibreSSL | 3.9.2 | TLS for package fetching | autotools configure + make |
| 5 | libarchive | 3.7.4 | .xbps archive handling | autotools configure + make |
| 6 | xbps | 0.60.7 | Package manager | custom configure + make (heavy patching) |

Sources downloaded to `vendor/` once, cached. Libs installed to
`build/deps/{lib,include,lib/pkgconfig}`.

---

## Cross-Compilation Setup

| Tool | Value |
|------|-------|
| CC | `aarch64-linux-musl-gcc` |
| AR | `aarch64-linux-musl-ar` |
| RANLIB | `aarch64-linux-musl-ranlib` |
| Make | GNU make (Homebrew or system) |
| pkg-config | system, with `PKG_CONFIG_LIBDIR` pointing to `build/deps/lib/pkgconfig` |

Critical linker flags: `-static -Wl,--entry=_start`

---

## build.rs Phases

### Phase 1: Toolchain detection
Finds GNU make, cross-compiler, AR/RANLIB tools.

### Phase 2: Dependency builds (each skipped if artifact exists)

Each dependency is downloaded, extracted, configured, and built with the
cross-compiler. Key adaptations per library:

- **zlib** — Makefile patched: `AR=libtool` → `AR=aarch64-linux-musl-ar`
- **lz4** — Only static lib built; headers/pc installed manually
- **zstd** — Individual make targets to avoid `install-shared` (macOS dylib failure)
- **LibreSSL** — `--disable-tests --disable-shared`
- **libarchive** — `--without-openssl --without-cng --disable-bsdtar/cpio/cat/unzip`;
  `.pc` file patched to add `-lz` to `Libs.private`

### Phase 3: Re-index archives
`aarch64-linux-musl-ranlib` run on every `.a` in `build/deps/lib/`.
Required because some build systems produce archives with symbol tables
the cross-linker cannot read.

### Phase 4: Build xbps
1. Run `./configure --host=aarch64-unknown-linux-musl --enable-static`
2. Patch `config.mk`: inject LDFLAGS/CFLAGS, strip PIE flags, strip `-l*` from LDFLAGS
3. Patch `lib/Makefile`: build only `libxbps.a` (skip `.so`)
4. Patch `mk/prog.mk`: build only `.static` binaries, install as base name,
   wrap STATIC_LIBS with `--start-group`/`--end-group`
5. `make DESTDIR={staging} install`

### Phase 5: Package
Create `dist/xbps.tar` (ustar format) and copy to `bootstrap/archives/`.

---

## Output Binaries

All 16 installed to `usr/bin/` inside the tar:

```
xbps-install      xbps-remove       xbps-query        xbps-pkgdb
xbps-reconfigure  xbps-alternatives xbps-create        xbps-rindex
xbps-dgraph       xbps-uhelper      xbps-checkvers     xbps-fbulk
xbps-fetch        xbps-digest       xbps-uchroot       xbps-uunshare
```

Format: ELF 64-bit LSB executable, ARM aarch64, statically linked (~3.4 MB each).

---

## Installation in Akuma

```sh
pkg install xbps
```

---

## Patterns Reused

| Pattern | From |
|---------|------|
| Lib Trick (`src/lib.rs`) | `dash/`, `make/`, `sbase/` |
| Vendor download cache | `dash/` |
| GNU Make path detection | `sbase/`, `dash/` |
| `-Wl,--entry=_start` | `dash/` |
| Makefile patching | `dash/` |
| `COPYFILE_DISABLE=1` tar | `sbase/` |
| Bootstrap archive copy | `sbase/`, `tcc/` |
