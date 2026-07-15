# musl libc

musl is the userspace C library. The kernel aims to run **unmodified
musl-static and musl-dynamic binaries** compiled for Linux AArch64; it does
not ship its own libc. For the syscall ABI that makes this possible, see
[`linux-compat.md`](linux-compat.md); for per-family syscall detail, see
[`../subsystems/syscalls.md`](../subsystems/syscalls.md).

> **Stability: A (stable).** musl bring-up was the Feb–Mar 2026 syscall-gap
> crisis, resolved and dormant since. ABI compatibility is the settled
> invariant. The recurring lesson: **musl (and Go, and c-ares) branch hard on
> `errno` values** — a syscall returning the wrong errno silently breaks a
> far-off code path (e.g. c-ares treats `fcntl(F_SETFD)` `EOPNOTSUPP` as
> fatal).

## What musl expects of the kernel

| musl need | Kernel provision | Source |
|---|---|---|
| Linux AArch64 syscall numbers | asm-generic table, x8 = nr | `src/syscall/mod.rs:177+` |
| `-errno` return on failure | `neg_errno(i32) -> u64` | `src/syscall/mod.rs` |
| `TPIDR_EL0` = userspace TLS | saved/restored across context switch | scheduler context switch |
| `TPIDRRO_EL0` = kernel-tracked TID | set per-thread by the kernel | — |
| `__init_tls` runs before user code | auxv (`AT_PHDR`/`AT_RANDOM`/`AT_PAGESZ`/`AT_ENTRY`) + PT_INTERP mapping | `crates/akuma-exec/src/elf/mod.rs` |
| `getrandom` for stack canaries / ASLR | nr 278 | `src/syscall/proc.rs` |
| `/dev/urandom` for crypto seed | `DevUrandom` fd | see `archive/ON_DEMAND_ELF_LOADER.md` |
| `clock_gettime(CLOCK_REALTIME/MONOTONIC)` | nr 113, ns resolution | `src/syscall/time.rs` |

## posix_spawn and CLONE_VFORK

musl `posix_spawn` (used by rustc/libstd subprocess spawn, by `git`, by
`cargo`) issues `clone(CLONE_VFORK | CLONE_VM)`. The kernel routes that flag
combination to the **vfork fast path** (`vfork_process`, gated by
`VFORK_FASTPATH_ENABLED`): the child shares the parent's page tables
(`new_shared(parent_l0)`) with **no CoW copy and no parent TLB flush**, and
the parent blocks until the child `execve`s or `_exit`s. On `execve`,
`replace_image` drops the shared view — the parent takes zero CoW faults. See
[`../subsystems/syscalls/proc.md`](../subsystems/syscalls/proc.md) and
[`../subsystems/memory.md`](../subsystems/memory.md) "CoW fork".

`CLONE_VM | CLONE_FILES` (plain `pthread_create`) shares the fd table via
`Arc::clone` — see [`linux-compat.md`](linux-compat.md) "Shared fd tables".

## musl-specific quirks that bit us

| Quirk | What breaks | Kernel handling |
|---|---|---|
| `posix_spawn` = `CLONE_VFORK\|CLONE_VM` | stale-TTBR0 in `vfork_process` → `git clone` wedges | all three call sites (`clone_thread`/`fork_process`/`vfork_process`) set `child_ctx.ttbr0 = new_proc.address_space.ttbr0()` — FIXED |
| c-ares (git's DNS) calls `fcntl(F_SETFD)` | rump `fcntl` returned `EOPNOTSUPP` → c-ares treats as fatal → DNS dead | `F_GETFD`/`F_SETFD` are no-op success — FIXED (see [`../runbooks/debug-devbox.md`](../runbooks/debug-devbox.md)) |
| Resolver expects `RESOLVE_HOST` (nr 300) or getaddrinfo-over-socket | musl resolver uses UDP sockets + `/etc/resolv.conf` | native stack does UDP; `RESOLVE_HOST` is an Akuma-private shortcut for libakuma |
| TLS access (`mrs TPIDR_EL0; ldr [x0, #off]`) before `__init_tls` | FAR = TLS offset, looks like NULL deref | fault dump prints `TPIDR_EL0`; if 0, it's pre-`__init_tls` |
| `tkill` must hit the target TID, not the caller | early `tkill` killed the caller | `src/syscall/signal.rs` delivers to the named TID |

## Dynamic linker

The dynamic linker is `ld-musl-aarch64.so.1`, mapped at `interp_base =
0x3000_0000`. It is loaded via `read_file()` (always < 16 MB). At runtime it
`openat`s libraries under `/lib`/`/usr/lib` (ext2), parses `PT_INTERP`-style
relocations itself, and `mmap`s them into the dynamic VA window. The kernel
does **not** do dynamic relocations for PIE binaries (ET_DYN self-relocate);
it only does static relocations for the (rare) non-PIE ET_EXEC case via the
`elf` crate.

JIT cache coherency for dynamic code (bun/JSC): `SCTLR_EL1.UCI=1` lets
`DC CVAU`/`IC IVAU`/`MRS CTR_EL0` run in EL0 directly; the EC=0x18 trap handler
emulates them as fallback (see `archive/ON_DEMAND_ELF_LOADER.md`).

## Sourcing musl on Akuma

musl is **not built in-tree**. It is sourced from Alpine's apk:

- Host cross-build (`userspace/tcc/build.rs`) downloads the pinned Alpine
  aarch64 `musl-dev` apk and extracts `usr/include` to cross-compile tcc.
- On Akuma: `apk add musl-dev` installs the libc + crt objects + headers
  (same version the host build pulled).
- The kernel ships no musl sysroot of its own (`libc.tar` was retired).
- tcc ships only `libtcc1.tar` (`libtcc1.a` + tcc internal headers); combined
  with `apk add musl-dev` that is the complete tcc toolchain.

## Cross-references

- ABI contract: [`linux-compat.md`](linux-compat.md).
- errno compliance detail: `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`.
- ELF/auxv/PT_INTERP: [`linux-compat.md`](linux-compat.md) "ELF loading".
- Toolchain bring-up history (rustc, go, bun): see the per-binary
  `archive/*_MISSING_SYSCALLS.md` docs.

## Background

- `archive/MUSL_COMPATIBILITY.md` — musl/TCC integration and ABI requirements.
- `archive/SYSCALL_HARDENING.md`, `archive/SYSCALL_ERRNO_COMPLIANCE_CHANGES.md`.
- `archive/ON_DEMAND_ELF_LOADER.md` — dynamic linker window, JIT cache, TLS.
- `archive/OPTIONAL_SMOLTCP.md` — c-ares `F_SETFD`, posix_spawn fixes.
- `userspace/libakuma/docs/SYSCALLS.md` — userspace syscall wrappers.
