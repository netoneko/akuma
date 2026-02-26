# XBPS Build Notes

## What Was Set Up

A Cargo package at `userspace/xbps/` that cross-compiles xbps and all its
dependencies for `aarch64-linux-musl` (static), following the Lib Trick
pattern from `userspace/README.md`.

**Files created:**
- `Cargo.toml` — library crate (no binary, avoids Cargo conflict)
- `src/lib.rs` — `#![no_std]` placeholder (Lib Trick)
- `build.rs` — all build logic
- `xbps/` — git submodule: `void-linux/xbps` at tag `0.60.7`
- `.gitignore` — excludes `vendor/`, `build/`, `dist/`

**Files modified:**
- `userspace/Cargo.toml` — added `"xbps"` member
- `userspace/build.sh` — added `"xbps"` to build order + archive copy

---

## Dependency Chain

xbps requires five static libraries built in order:

```
zlib 1.3.1  →  lz4 1.9.4  →  zstd 1.5.5  →  LibreSSL 3.9.2  →  libarchive 3.7.4  →  xbps
```

All downloaded to `vendor/` on first build (cached; only downloaded once).
Installed to `build/deps/{lib,include,lib/pkgconfig}`.

**Key design:** xbps configure uses `pkg-config` exclusively to find its
deps — not custom env vars. `PKG_CONFIG_LIBDIR` is set to
`build/deps/lib/pkgconfig` so it only finds our cross-compiled libs.

---

## Issues Found and Fixed During Build

### 1. Wrong zlib URL
`https://zlib.net/zlib-1.3.1.tar.gz` returns 404.
**Fix:** Use `https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz`

### 2. macOS `libtool` can't archive ELF objects (zlib)
zlib's configure on macOS sets `AR=libtool ARFLAGS=-o`. The macOS
`libtool` rejects ELF `.o` files from `aarch64-linux-musl-gcc`.
**Fix:** Patch zlib's generated Makefile: `AR=libtool` → `AR=aarch64-linux-musl-ar`,
`ARFLAGS=-o` → `ARFLAGS=rcs`.

### 3. lz4 always builds shared library
lz4's default target builds both static and shared. The shared build uses
macOS dylib flags that the cross-compiler rejects.
**Fix:** Build only `liblz4.a` explicitly, manually install .a, headers, and .pc.

### 4. zstd `install` target includes `install-shared`
Same dylib issue. The `install` target depends on `install-shared` which
fails, preventing `install-includes` from running (headers never installed).
**Fix:** Call `install-static`, `install-pc`, `install-includes` individually.

### 5. Archive index incompatibility (llvm-ar vs musl-ld)
Archives created by `llvm-ar` (from Homebrew LLVM) have a symbol table
format that `aarch64-linux-musl-ld` cannot read, causing "undefined
reference" errors even though the symbols are present.
**Fix:** Run `aarch64-linux-musl-ranlib` on all `.a` files after building
to regenerate compatible archive indexes.

### 6. libarchive.pc missing zlib dependency
libarchive's generated `.pc` file omits `-lz` from `Libs.private`, causing
`pkg-config --libs --static libarchive` to not include zlib.
**Fix:** Patch `libarchive.pc` after install to add `-lz` to `Libs.private`.

### 7. xbps configure uses pkg-config, not custom env vars
Initial plan passed `ZLIB_LIBS`, `SSL_LIBS` etc. These don't exist.
**Fix:** Set `PKG_CONFIG_LIBDIR` to our deps pkgconfig dir.

### 8. xbps builds dynamic binaries by default
Without patches, xbps builds both `libxbps.so` and dynamic binaries.
**Fix:** Patch `lib/Makefile` to only build `libxbps.a`, patch `mk/prog.mk`
to only build `.static` binaries (installed without the `.static` suffix).

### 9. PIE conflicts with static linking
xbps configure enables `-fPIE` and `-pie` which conflict with `-static`.
**Fix:** Strip PIE flags from `config.mk` during post-configure patching.

### 10. LDFLAGS library flags cause link order issues
xbps configure appends `-larchive -lssl -lz` to LDFLAGS (from pkg-config).
These appear before STATIC_LIBS on the link line, confusing the linker.
**Fix:** Strip `-l*` flags from LDFLAGS in config.mk; libraries are already
in STATIC_LIBS in the correct order. Also wrap STATIC_LIBS with
`--start-group`/`--end-group` to handle circular dependencies.

### 11. xbps `--host` triplet OS detection
`--host=aarch64-linux-musl` is parsed as OS=`musl`, skipping Linux-specific
defines (`_XOPEN_SOURCE`, `_FILE_OFFSET_BITS`).
**Fix:** Use `--host=aarch64-unknown-linux-musl` (4-part triplet).

---

## Status

zlib ✅ builds and installs correctly
lz4 ✅ static lib built and manually installed
zstd ✅ builds with individual install targets
LibreSSL ✅ builds with cross-compilation
libarchive ✅ builds with patched .pc file
xbps ✅ all 16 static binaries built and packaged

**Output:** `dist/xbps.tar` (56 MB) → `bootstrap/archives/xbps.tar`
Contains 16 statically-linked ELF64 aarch64 binaries in `usr/bin/`.

---

## Cross-Compilation Notes

| Tool | Value |
|------|-------|
| CC | `aarch64-linux-musl-gcc` |
| AR | `aarch64-linux-musl-ar` (preferred for compatible archive indexes) |
| RANLIB | `aarch64-linux-musl-ranlib` |
| Make | `/opt/homebrew/opt/make/libexec/gnubin/make` or `make` |
| Target | `aarch64-linux-musl` (static, `--entry=_start`) |
| pkg-config | system `pkg-config` with `PKG_CONFIG_LIBDIR` override |

**Important:** Archives MUST be re-indexed with `aarch64-linux-musl-ranlib`
before linking. The `reindex_archives()` step in build.rs handles this.
