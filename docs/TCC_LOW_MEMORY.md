# Low-Memory TCC: Optimization, Toolchain, and the Compile Floor

This documents the June 2026 work that took in-VM C compilation from an apk
dynamic tcc (~6 MB floor) down to the **kernel boot floor (~4.5 MB)**, and
settled what does and doesn't move that floor.

## TL;DR

- We compile with **our own** tcc (`userspace/tcc/`), a single **static** ELF —
  not Alpine's dynamic `/usr/bin/tcc` stub (`libtcc.so` + musl loader).
- Size-optimizing that binary cut it **603 KB → 291 KB** (PT_LOAD memsz
  ~500 KB → ~383 KB).
- **Measured floor: 4.5 MB** for both a `printf` hello and a bare-`write` hello,
  on the `extreme-size` kernel. 4.0 MB fails to **boot** — so the floor is the
  kernel, not the compiler or libc.
- musl is sourced from **apk** on both sides (headers at build time, libc on
  Akuma). No in-tree musl build, no shipped `libc.tar`.

## tcc binary optimization

`userspace/tcc/build.rs` builds tcc's C sources via the `cc` crate. Changes:

- Forward cargo's size `OPT_LEVEL` (`s`/`z`) straight to the C compiler
  (`-Os`/`-Oz`) instead of remapping it to `-O3`.
- `-ffunction-sections -fdata-sections` + a `--gc-sections` link arg so the
  linker drops tcc codegen paths that are never reached.
- The release profile already strips (`strip = true`).

Result: `bootstrap/bin/tcc` is **291,384 bytes**, statically linked, stripped.

## Toolchain: musl from apk, only tcc is ours

`build.rs` downloads the pinned Alpine aarch64 `musl-dev` apk (cached in
`userspace/tcc/vendor/`) and uses its `usr/include` to cross-compile tcc. On
Akuma, `apk add musl-dev` supplies `crt1.o`/`crti.o`/`crtn.o`, `libc.a`, and the
POSIX headers.

We ship exactly one sysroot artifact, **`libtcc1.tar`** = `usr/lib/tcc/libtcc1.a`
**plus** tcc's internal headers (`usr/lib/tcc/include/`, incl. `tccdefs.h`). The
old full-sysroot `libc.tar` was retired. See [MUSL_COMPATIBILITY.md](MUSL_COMPATIBILITY.md)
and [../userspace/tcc/docs/LIBTCC1.md](../userspace/tcc/docs/LIBTCC1.md).

Compile invocation (output must land on `PATH`, e.g. `/bin`, to be runnable):

```sh
apk add musl-dev
busybox tar xf /archives/libtcc1.tar -C /
tcc -static -B /usr/lib/tcc -o /bin/prog prog.c
```

**`-static` is required.** Our tcc has no compiled-in ELF interpreter, so the
default (dynamic) link produces a binary needing the musl loader Akuma lacks →
SIGSEGV. `-static` links `libc.a` and runs.

## The compile floor (measured)

`scripts/our_tcc_floor.py` — serial, `snapshot=on`, one source per boot on the
`extreme-size` kernel; disk pre-staged with our tcc, apk musl-dev, and
`libtcc1.tar`.

| RAM | `hello.c` (printf) | `hello_stripped.c` (write) |
|----:|:---:|:---:|
| 5.0 MB | ✅ | ✅ |
| **4.5 MB** | ✅ | ✅ |
| 4.0 MB | ❌ kernel won't boot | ❌ kernel won't boot |

### What this proves

1. **The floor is the kernel boot floor (~4.5 MB), not tcc or libc.** The
   compile fits in whatever is left above boot (free-RAM low-water hit ~1 MB at
   4.5 MB and still succeeded).
2. **`printf` vs `write` floor identically.** `hello.c` drags in far more of the
   11 MB musl `libc.a` than `hello_stripped.c`, yet both floor at 4.5 MB — so
   **libc's contribution to the floor is ~zero**.
3. Therefore a **smaller libc** (uClibc, dietlibc, a hand-rolled mini-stub) would
   **not** lower the floor. The next lever is the kernel boot floor itself
   (thread-stack pool, heap seed, reserved) — see
   [LOW_MEMORY_ENVIRONMENT.md](LOW_MEMORY_ENVIRONMENT.md).

### Hard constraint discovered

tcc's AArch64 backend has **no inline assembler** (`error: ARM asm not
implemented`). So freestanding raw-syscall C, `nolibc`, and header-only libcs are
impossible with tcc — any "no libc" path needs a **host-assembled** syscall stub
`.a`. (Deferred; it would be for self-containment, not for the floor.)

### When the box is too small (e.g. meow + tcc at 4.5 MB)

tcc alone fits at 4.5 MB, but **meow + tcc** together need 5 MB. Below that the
kernel used to abort under memory exhaustion; it now **OOM-kills the offending
process and survives** (PMM emergency reserve — see the *OOM hardening* section
in [LOW_MEMORY_ENVIRONMENT.md](LOW_MEMORY_ENVIRONMENT.md)).
