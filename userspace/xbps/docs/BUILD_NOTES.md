# XBPS Build Notes

## What Was Set Up

A Cargo package at `userspace/xbps/` that cross-compiles xbps and all its
dependencies for `aarch64-linux-musl` (static), following the Lib Trick
pattern from `userspace/README.md`.

**Files created:**
- `Cargo.toml` — library crate (no binary, avoids Cargo conflict)
- `src/lib.rs` — empty placeholder
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
`https://zlib.net/fossils/zlib-1.3.1.tar.gz` times out.
**Fix:** Use `https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz`

### 2. macOS `libtool` can't archive ELF objects (zlib)
zlib's configure on macOS sets `AR=libtool ARFLAGS=-o`. The macOS
`/Library/Developer/CommandLineTools/usr/bin/libtool` rejects ELF `.o`
files from `aarch64-linux-musl-gcc`.
**Fix:** After zlib configure, patch the generated `Makefile` to replace
`AR=libtool` → `AR=llvm-ar` (or `aarch64-linux-musl-ar`) and
`ARFLAGS=-o` → `ARFLAGS=rcs`. Also pass `AR`/`RANLIB` env to all dep builds.

### 3. lz4 always builds shared library
lz4's `lib` Makefile default target (`lib: liblz4.a liblz4`) unconditionally
builds both static and shared. The shared build uses `-install_name`,
`-compatibility_version`, `-current_version` (macOS dylib linker flags)
which `aarch64-linux-musl-gcc` doesn't support. `BUILD_SHARED=no` only
affects the install step, not the build step.
**Fix:** Build only `liblz4.a` explicitly (`make liblz4.a`), then manually
copy the `.a` and headers to `build/deps/` and write a `liblz4.pc` file.

### 4. xbps configure uses pkg-config, not custom env vars
Initial plan passed `ZLIB_LIBS`, `SSL_LIBS`, `LIBARCHIVE_LIBS` env vars.
These don't exist in xbps's configure script — it only reads from
`pkg-config`.
**Fix:** Set `PKG_CONFIG_LIBDIR=build/deps/lib/pkgconfig` (overrides all
system pkg-config paths) when configuring xbps.

### 5. xbps builds dynamic binaries by default
Without `--enable-static`, xbps produces executables that link against
shared libs (which don't exist on Akuma).
**Fix:** Add `--enable-static` to xbps configure invocation.

---

## Status at Time of Writing

zlib ✅ builds and installs correctly
lz4 ✅ static lib built and manually installed
zstd 🔄 build started but interrupted — outcome unknown
LibreSSL ⏳ not yet reached
libarchive ⏳ not yet reached
xbps ⏳ not yet reached

---

## What Likely Still Needs Attention

- **zstd shared lib**: Same dylib issue as lz4 may occur. Use `make -C lib`
  with `BUILD_SHARED=no` or build only `libzstd.a` explicitly.
- **LibreSSL configure**: May fail if it tries to run cross-compiled test
  binaries. Adding `--build=$(HOST_TRIPLET)` should suppress that.
- **libarchive**: Needs `PKG_CONFIG_LIBDIR` set so it finds lz4/zstd `.pc`
  files. Already in build.rs.
- **xbps Makefile patching**: The xbps configure writes `LDFLAGS` into
  `config.mk`; `patch_ldflags_in_config()` in build.rs handles this but
  has not been tested yet.
- **Packaging**: `tar --format=ustar` step and bootstrap copy not yet
  reached.

---

## Cross-Compilation Notes

| Tool | Value |
|------|-------|
| CC | `aarch64-linux-musl-gcc` |
| AR | `llvm-ar` (Homebrew) or `aarch64-linux-musl-ar` |
| RANLIB | `llvm-ranlib` (Homebrew) or `aarch64-linux-musl-ranlib` |
| Make | `/opt/homebrew/opt/make/libexec/gnubin/make` or `make` |
| Target | `aarch64-linux-musl` (static, `--entry=_start`) |
| pkg-config | system `pkg-config` with `PKG_CONFIG_LIBDIR` override |
