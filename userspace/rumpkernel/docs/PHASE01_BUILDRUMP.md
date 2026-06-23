# Phases 0/1 — buildrump cross-build: **DONE** (aarch64-musl librump\* built)

Status: **complete.** `librump*.a` static archives for `aarch64` (musl) are built
and verified — 304 archives including the full NetBSD TCP/IP stack. Built in a
**Linux container** (not macOS), which sidesteps the 2016-source-vs-modern-clang
host-tool failures (plan §7 #1/#2).

This is the userspace half of the port (the `librump*.a` an Akuma program links
to host a NetBSD TCP/IP stack). The **kernel** half (the raw L2 `/dev/net/tap0`)
is also done — see [PHASE3_KERNEL_TAP.md](PHASE3_KERNEL_TAP.md).

## How to build

```sh
cd userspace/rumpkernel
./build.sh checkout      # ~375 MB clone of pinned src-netbsd (once)
./docker-build.sh        # build librump*.a in an Alpine arm64 container
```

Artifacts (git-ignored): `obj/dest.stage/usr/lib/librump*.a`. Key ones for M1:
- `librump.a` (9.5 MB) — core rump kernel
- `librumpnet.a` (1.7 MB) — net core
- `librumpnet_netinet.a` (84 KB) — **the NetBSD TCP/IP stack**
- `librumpnet_config.a` (640 KB) — netconfig (DHCP oneshot)
- `librumpvfs.a` — VFS faction

Verified arch: `file` on a member → `ELF 64-bit LSB relocatable, ARM aarch64`.

## Why a Linux container

The pinned NetBSD source is from 2016 (NetBSD 7.99.34). Its *host tools*
(nbmake, compat, mandoc, lex, …) bootstrap with the **host** compiler before any
target compilation. Modern Apple clang 17 on macOS rejects 2016-era C outright
(implicit-function-declaration / implicit-int are hard errors), and there is no
old clang/gcc on the macOS host. A Linux container fixes the *environment*:

- **arm64 host → Alpine arm64 is musl-native on aarch64** → a *native* build in
  the container *is* a build for `aarch64-linux-musl` (Akuma's target). No cross
  toolchain, ABI matches `userspace/build.sh`'s `aarch64-linux-musl-gcc` output.
- `gcc` (not clang) as the host compiler — laxer defaults.
- `apk add linux-headers` provides `<linux/if_tun.h>` (the macOS musl sysroot
  lacked it).

`docker-build.sh` runs `build.sh` inside `alpine:3.20` with a small `gcc`/`g++`
wrapper that makes the 2016 source compile on a 2026 toolchain.

## The three compiler-era fixes (in the gcc/g++ wrapper)

All in `docker-build.sh`'s wrapper, discovered by iterating the build:

1. **`-fcommon`** — gcc 10+ defaults to `-fno-common`, so the 2016 source's
   tentative-definition globals (e.g. `debug_file` in nbmake) collide at link
   ("multiple definition"). `-fcommon` restores the old merging.
2. **trailing `-Wno-error`** — the NetBSD build compiles with `-Werror`, and
   modern gcc flags much 2016 code (`cast-function-type`, `uninitialized`, macro
   `redefined`, …). The flag must come **after** `"$@"` to win over NetBSD's own
   `-Werror` (a prefix flag loses to the later one). Downgrades them to warnings.
3. **BSD cdefs shim** (`-include`) — musl's headers lack `__BEGIN_DECLS` /
   `__END_DECLS` (glibc and BSD provide them), which NetBSD's own headers (e.g.
   `include/regex.h`) assume → `expected ';' before 'int'` in host tools. A tiny
   force-included shim defines them if absent.

(`build.sh` also passes `-fcommon` + the implicit-decl relaxations to the target
build for the macOS path; the container wrapper is the robust catch-all.)

## virtif (Phase 4): reuse their kernel driver, write our own backend glue

virtif has **two separable pieces**, and we treat them differently — the same
"use their kernel code, write our own host glue" line as `rumpuser`:

- **virtif kernel driver** (`if_virt.c`, compiled into `librumpnet_virtif.a`) —
  the network interface *inside* the NetBSD stack that DHCP/IP/TCP bind to. **We
  reuse this** (it is the NIC the rump stack sees; nothing substitutes for it).
- **packet backend** (`rumpcomp_user.c` — the `rumpcomp_virtif_{create,send,recv,
  destroy}` hypercalls the driver calls to move bytes on/off the wire). **We write
  our own** thin glue over Akuma syscalls (`open("/dev/net/tap0")` + `read`/`write`),
  rather than rump's stock Linux TUN/TAP backend.

**Why our own backend, not the stock Linux one:** the backend is a tiny published
hypercall contract (~4 functions) — writing it is the same kind of work as the Rust
`rumpuser`, and consistent with that decision. The stock backend's only draw is
being "tested", but it would only ever run against **our** `/dev/net/tap0`, which is
itself a *mimicry* of Linux TUN/TAP — so it tests our mimicry's fidelity, not
real-world behaviour, while costing us the full TUN/TAP ABI (`TUNSETIFF` /
`SIOCSIFFLAGS` / `SIOCGIFHWADDR` / `/dev/net/tun` open semantics) plus the fight to
force `RUMP_VIRTIF=yes` under `-k` (`evalplatform` is skipped at `buildrump.sh:1639`,
and the top-level `RUMP_VIRTIF=no` clobbers the env var). Our glue avoids all of it.

**Side effect on Phase 3:** the kernel tap stays exactly as built (open + raw-frame
read/write), but its `TUNSETIFF` no-op becomes *optional* rather than load-bearing —
`/dev/net/tap0` can just be a clean packet device, not a TUN/TAP impersonation.

`librumpnet_virtif.a` was not built in this Phase-1 run (the `-k`/`RUMP_VIRTIF`
interaction above). Phase 4 builds just `if_virt.c` into it and links our own
`rumpcomp_user.o` — mirroring how the Rust `rumpuser` replaces NetBSD's C
librumpuser. Phase 1's deliverable (core + TCP/IP libraries) is complete regardless.

## Next (Phase 2 / Phase 4)

- **Phase 2**: Rust `rumpuser/` staticlib exporting the `rumpuser_*` C ABI
  (`RUMPUSER_VERSION 17`, from `src-netbsd/sys/rump/include/rump/rumpuser.h`) over
  `libakuma`, linked in place of NetBSD's C librumpuser. Link a trivial app
  against `librump.a` + the Rust rumpuser that calls `rump_init()`.
- **Phase 4**: build `librumpnet_virtif` with an Akuma `rumpcomp_user` backend
  bound to `/dev/net/tap0`.
