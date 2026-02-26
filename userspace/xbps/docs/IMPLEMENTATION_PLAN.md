# XBPS for Akuma OS — Implementation Plan

## Overview

This document describes how XBPS (X Binary Package System, the Void Linux package manager) is cross-compiled and packaged for Akuma OS as a statically-linked `aarch64-linux-musl` binary set.

**Source:** https://github.com/void-linux/xbps
**Location in repo:** `userspace/xbps/xbps/` (git submodule)
**Build entry point:** `userspace/xbps/build.rs`
**Output:** `userspace/xbps/dist/xbps.tar` → `bootstrap/archives/xbps.tar`

---

## Architecture

The `xbps` Cargo package follows the **Lib Trick** pattern from `userspace/README.md`:
- `src/lib.rs` is empty — prevents Cargo from building a competing binary
- `build.rs` does all the heavy lifting: downloading deps, cross-compiling, packaging

```
userspace/xbps/
├── Cargo.toml           Cargo package (library crate, no binary)
├── build.rs             All build logic
├── src/lib.rs           Empty (Lib Trick)
├── xbps/                Git submodule: void-linux/xbps
├── vendor/              Downloaded dependency sources (gitignored, cached)
├── build/deps/          Installed static libraries (gitignored, cached)
└── dist/xbps.tar        Final package for pkg install
```

---

## Dependency Chain

xbps requires the following static libraries, each cross-compiled for `aarch64-linux-musl` before xbps itself is built:

| Library | Version | Purpose | Build method |
|---------|---------|---------|-------------|
| zlib | 1.3.1 | Compression | configure + make |
| lz4 | 1.9.4 | Fast compression (libarchive) | make -C lib |
| zstd | 1.5.5 | Compression (libarchive) | make install |
| LibreSSL | 3.9.2 | TLS/HTTPS for package fetching | configure + make |
| libarchive | 3.7.4 | Package archive (`.xbps`) handling | configure + make |

All sources are downloaded to `vendor/` on first build and cached there. Rebuilt libs install to `build/deps/{include,lib}`.

**Build order matters:** zlib → lz4 → zstd → LibreSSL → libarchive → xbps.

---

## Cross-Compilation Setup

**Compiler:** `aarch64-linux-musl-gcc`
**Archiver:** `llvm-ar` (Homebrew) or `aarch64-linux-musl-ar`
**Build system:** GNU Make (Homebrew `/opt/homebrew/opt/make/libexec/gnubin/make` or `make`)

**Critical linker flags:**
```
-static                  Static linking — no dynamic libraries available in Akuma
-Wl,--entry=_start       Explicit entry point — required for custom kernel targets
```

---

## Build Process (build.rs walkthrough)

### Phase 1: Toolchain detection
Detect GNU make and llvm-ar paths (Homebrew vs system fallback).

### Phase 2: Dependency builds (all skipped if artifact already present)

**zlib:**
```bash
CC=aarch64-linux-musl-gcc CFLAGS="-Os -fPIC" \
  ./configure --prefix={build/deps} --static
make install
```

**lz4:**
```bash
CC=aarch64-linux-musl-gcc \
  make -C lib install PREFIX={build/deps}
```

**zstd:**
```bash
CC=aarch64-linux-musl-gcc \
  make install PREFIX={build/deps}
```

**LibreSSL:**
```bash
CC=aarch64-linux-musl-gcc CFLAGS="-Os -fPIC" \
  ./configure --host=aarch64-linux-musl \
              --prefix={build/deps} \
              --disable-shared --enable-static --disable-tests
make install
```

**libarchive** (links against zlib, lz4, zstd):
```bash
CC=aarch64-linux-musl-gcc \
  CFLAGS="-Os -fPIC -I{build/deps/include}" \
  LDFLAGS="-L{build/deps/lib}" \
  ./configure --host=aarch64-linux-musl \
              --prefix={build/deps} \
              --disable-shared --enable-static \
              --with-zlib --with-lz4 --with-zstd \
              --without-bz2lib --without-libb2 --without-iconv \
              --without-lzma --without-lzo2 --without-xml2 --without-expat \
              --disable-bsdtar --disable-bsdcpio --disable-bsdcat
make install
```

### Phase 3: Build xbps

```bash
CC=aarch64-linux-musl-gcc \
  CFLAGS="-Os -I{build/deps/include}" \
  LDFLAGS="-static -L{build/deps/lib} -Wl,--entry=_start" \
  ZLIB_CFLAGS="-I{build/deps/include}"   ZLIB_LIBS="{build/deps/lib/libz.a}" \
  SSL_CFLAGS="-I{build/deps/include}"    SSL_LIBS="{build/deps/lib/libssl.a ...}" \
  LIBARCHIVE_CFLAGS="-I{build/deps/include}" LIBARCHIVE_LIBS="{build/deps/lib/libarchive.a}" \
  ./configure --prefix=/usr --host=aarch64-linux-musl --sysconfdir=/etc --disable-tests
make DESTDIR={staging} install
```

After `configure`, generated Makefiles are patched to prevent LDFLAGS override (same pattern as `dash/build.rs`).

### Phase 4: Package

```bash
COPYFILE_DISABLE=1 tar --no-xattrs --format=ustar \
  -cf dist/xbps.tar -C {staging} usr
cp dist/xbps.tar ../../bootstrap/archives/xbps.tar
```

---

## Installation inside Akuma

```sh
pkg install xbps
```

This installs the xbps binaries into `/usr/bin/`:
- `xbps-install` — install packages
- `xbps-remove` — remove packages
- `xbps-query` — query package database
- `xbps-rindex` — manage package repositories
- `xbps-pkgdb` — package database management

---

## Patterns Reused

| Pattern | From |
|---------|------|
| Lib Trick (src/lib.rs) | `dash/`, `make/`, `sbase/` |
| Vendor download cache | `dash/` |
| GNU Make path detection | `sbase/`, `dash/` |
| `-Wl,--entry=_start` | `dash/` |
| Makefile LDFLAGS patching | `dash/` |
| `COPYFILE_DISABLE=1` tar | `sbase/` |
| Bootstrap archive copy | `sbase/`, `tcc/` |
| llvm-ar/llvm-ranlib detection | `tcc/` |

---

## Troubleshooting

**xbps configure fails to find libarchive/ssl/zlib:**
Check that `build/deps/lib/libarchive.a` etc. exist. If not, delete `build/` and rebuild.

**"missing entry point" or binary crashes:**
Ensure `-Wl,--entry=_start` is in LDFLAGS and was not overridden by the Makefile. The Makefile patching step in `build.rs` handles this.

**LibreSSL configure fails on macOS with cross-compile errors:**
LibreSSL's configure may try to run cross-compiled test binaries. This should be suppressed by `--host=aarch64-linux-musl`. If it still fails, try adding `--build=x86_64-apple-darwin` or `--build=aarch64-apple-darwin`.

**Dependency libraries not found by libarchive:**
Ensure `CFLAGS` includes `-I{build/deps/include}` and `LDFLAGS` includes `-L{build/deps/lib}` when configuring libarchive.
