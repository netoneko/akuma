# libtcc1.a — TCC Runtime Library

## What is libtcc1.a?

`libtcc1.a` is TCC's internal runtime support library. It provides helper functions that the compiler emits calls to for operations the CPU cannot do in a single instruction — 128-bit multiplication, division, atomic operations, and AArch64 cache maintenance. Every binary TCC produces requires `libtcc1.a` at link time.

TCC searches for `libtcc1.a` in its library paths. On Akuma, these are configured as:

```
/usr/lib
/usr/lib/tcc
```

(Set via `CONFIG_TCC_LIBPATHS` in `src/config.h`.)

## The Problem

TCC (both Alpine's package and Akuma's build) fails with:

```
tcc: error: file 'libtcc1.a' not found
```

This happens because:

- **Alpine's `tcc` package (aarch64):** Does not ship `libtcc1.a`. The package build for AArch64 does not produce the runtime library, so `apk add tcc` gives you a compiler that cannot link.
- **Akuma's TCC:** `libtcc1.a` is built by `build.rs` and shipped in `libtcc1.tar`. Install it to `/usr/lib/tcc/` (see Installation below); the musl libc/crt it links alongside comes from `apk add musl-dev`.

## Build Artifacts

The TCC build (`build.rs`) produces a **single** tar archive — the musl sysroot
(`libc.tar`) was retired; musl is sourced from `apk add musl-dev` on Akuma.

| Artifact | Contents | Use Case |
|---|---|---|
| `libtcc1.tar` | `usr/lib/tcc/libtcc1.a` **plus** `usr/lib/tcc/include/` (tcc's internal headers: `tccdefs.h`, `stddef.h`, `stdarg.h`, …) | Everything our tcc needs on top of `apk add musl-dev`. Also patches an Alpine container whose tcc package is missing the runtime library. |

It is placed in `userspace/tcc/dist/` after building, and `build.sh` copies it to `bootstrap/archives/`.

> The tcc include dir **must** ride in `libtcc1.tar`: without `tccdefs.h` on the
> tcc lib path, every compile fails with `include file 'tccdefs.h' not found`.

## How libtcc1.a is Built

In `build.rs`, the runtime library is cross-compiled for AArch64 from TCC's own source:

1. `tinycc/lib/libtcc1.c` → `libtcc1_base.o` (integer helpers, builtins)
2. `tinycc/lib/lib-arm64.c` → `lib-arm64.o` (AArch64-specific: cache flush, 128-bit ops)
3. Both objects are archived into `libtcc1.a` using `llvm-ar`

## Installation

### On Akuma

Install the musl libc + crt + headers, then our tcc runtime + headers:

```sh
apk add musl-dev                 # crt1.o/crti.o/crtn.o, libc.a, POSIX headers
busybox tar xf /archives/libtcc1.tar -C /   # /usr/lib/tcc/{libtcc1.a,include}
```

Then compile with `tcc -static -B /usr/lib/tcc -o /bin/prog prog.c`.

### On Alpine Linux (aarch64)

Alpine's TCC package does not include `libtcc1.a`. To fix:

```sh
tar xf libtcc1.tar -C /
```

This places `libtcc1.a` at `/usr/lib/tcc/libtcc1.a`, which is where Alpine's TCC expects to find it.

Alternatively, build from source in the Alpine container:

```sh
apk add tcc musl-dev
cd /tmp && wget https://download.savannah.gnu.org/releases/tinycc/tcc-0.9.28rc.tar.bz2
tar xf tcc-0.9.28rc.tar.bz2 && cd tcc-0.9.28rc
tcc -c lib/lib-arm64.c -o lib-arm64.o -I. -Iinclude
tcc -c lib/libtcc1.c -o libtcc1.o -I. -Iinclude
tcc -ar /usr/lib/tcc/libtcc1.a libtcc1.o lib-arm64.o
```

## Static vs Dynamic Linking

TCC defaults to dynamic linking (using `libc.so`). On Akuma, which has no dynamic linker (`ld-musl-aarch64.so.1`), dynamically linked binaries crash — GOT/PLT entries for libc functions resolve to address 0.

**Always use `-static` on Akuma:**

```sh
tcc -static hello.c -o hello
```

On Alpine (which has a working dynamic linker), both modes work:

```sh
# Dynamic (default) — requires ld-musl-aarch64.so.1 at runtime
tcc hello.c -o hello_dynamic

# Static — standalone binary, no runtime dependencies
tcc -static hello.c -o hello_static
```

### Akuma TCC (`/bin/tcc`, v0.9.27)

```sh
# This works — static binary
/bin/tcc -static hello.c -o /tmp/hello
/tmp/hello
# Hello, world!

# This compiles but the binary crashes (no dynamic linker)
/bin/tcc hello.c -o /tmp/hello_dynamic
/tmp/hello_dynamic
# [exit code: -11]
```

### Alpine TCC (`/usr/bin/tcc`, v0.9.28rc)

```sh
# Dynamic (default) — works on Alpine, crashes on Akuma
tcc hello.c -o /tmp/hello
/tmp/hello
# Hello, world!

# Static — works everywhere
tcc -static hello.c -o /tmp/hello_static
/tmp/hello_static
# Hello, world!
```

## Debugging

Use `-vv` to see TCC's file access during compilation:

```sh
tcc -vv -static hello.c -o /tmp/hello
```

This prints every file TCC opens, helping diagnose missing sysroot components.

Akuma's TCC also logs `.a`, `.o`, and `crt` file accesses to stderr (implemented in `src/main.rs`):

```
tcc: open('/usr/lib/crt1.o') -> SUCCESS (fd=6)
tcc: open('/usr/lib/libtcc1.a') -> FAILED (-2)
tcc: open('/usr/lib/tcc/libtcc1.a') -> SUCCESS (fd=14)
```
