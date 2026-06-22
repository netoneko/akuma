# Phases 0/1 — buildrump cross-build: findings & state

Status: **in progress / de-risked**. The driver (`build.sh`) and the pinned
NetBSD source checkout work; the cross-build currently stalls in NetBSD's
**host-tool** bootstrap due to 2016-source vs. modern-Apple-clang
incompatibilities — exactly the plan's §7 risks #1 (toolchain divergence) and #2
(2016 source pin), now concrete.

This is the userspace half of the port (the `librump*.a` archives an Akuma
program links to host a NetBSD TCP/IP stack). The **kernel** half is already done
and verified — see [PHASE3_KERNEL_TAP.md](PHASE3_KERNEL_TAP.md).

## What works

- **`build.sh`** drives `buildrump.sh` with the `aarch64-linux-musl` cross
  toolchain. Commands: `checkout` | `build` | `host` | `clean`. Outputs to
  git-ignored `obj/` + `dest/`.
- **Source checkout** (`./build.sh checkout`): clones the pinned rump src-netbsd
  (rev `82f3a69`, NetBSD 7.99.34) into `src-netbsd/` (~375 MB, git-ignored).
- **Toolchain detection**: buildrump correctly resolves the cross tools and the
  target — `MACHINE: evbarm64`, `MACHINE_ARCH: aarch64`, `RUMPKERN_ONLY: yes`
  (our `-k`, so NetBSD's C librumpuser is skipped for the Rust one in Phase 2).

## Blockers found (host-tool build, on macOS Apple clang)

NetBSD bootstraps its own build tools (`nbmake`, `compat`, `mandoc`, …) with the
**host** compiler before cross-compiling anything. Modern Apple clang (≥16)
rejects 2016-era C that older compilers accepted:

1. **`tools/compat`** — `error: call to undeclared function 'mi_vector_hash'`
   (implicit function declarations are now hard errors).
   **Fixed** by passing `-V HOST_CFLAGS=-Wno-error=implicit-function-declaration`
   (and `HOST_CPPFLAGS`) — `-F CFLAGS=…` alone does **not** work: it only reaches
   the *target* cross-compile, not the host-tool build.
2. **`tools/mandoc`** — `config.h: conflicting types for '__builtin___strlcat_chk'
   / '__builtin___strlcpy_chk'`, `implicit int`. mandoc's autoconf `config.h`
   redeclares `strlcat`/`strlcpy` in a way that clashes with clang builtins.
   **Not yet resolved.** (mandoc is a man-page formatter — not needed for the
   rump libraries themselves; the lever is to skip it or relax its flags.)

Expect more of the same down the host-tool list. The realistic options, in
rough order of robustness:

- **Build on Linux**, not macOS — a Linux host with an older/looser default
  `cc` sidesteps most of these (the rump CI targets Linux). Cross-compile to
  aarch64-musl from there. **Recommended** for the actual library build.
- **Pin an older host clang/gcc** (e.g. via Homebrew `llvm@15` or `gcc`) so
  implicit-int/implicit-function-declaration stay warnings.
- **Per-tool flag relaxation** continuing the `HOST_CFLAGS` approach
  (`-Wno-error=implicit-int`, `-Wno-error=…`) — works but is whack-a-mole.
- **Skip unneeded host tools** (mandoc/man) via NetBSD `MK*=no` knobs if
  buildrump exposes them through `-V`.

## Reproduce

```sh
cd userspace/rumpkernel
./build.sh checkout      # ~375 MB clone (once)
./build.sh build         # cross-build; currently stops in tools/mandoc
```

`build.sh` already applies fix #1. Logs are verbose; the real error is usually a
`*.c:NNN: error:` line well above the `*** BUILD ABORTED ***` banner.

## Once the libraries build (Phase 1 exit → Phase 2/4)

`dest/lib/librump*.a` is the artifact set. Then:
- **Phase 2**: Rust `rumpuser/` staticlib exporting the `rumpuser_*` C ABI over
  `libakuma`, linked in place of NetBSD's C librumpuser.
- **Phase 4**: bind rump's virtif `rumpcomp_user` packet hypercall to the kernel
  **`/dev/net/tap0`** that Phase 3 already provides. Note `RUMP_VIRTIF` probed to
  **no** here because the musl sysroot lacks `<linux/if_tun.h>` — a small shim
  header will be needed to enable the stock Linux virtif backend (plan §7 #1).
