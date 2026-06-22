# rumpkernel — NetBSD rump kernel compatibility for Akuma

*With deep respect to NetBSD throughout the years.*

This subtree brings **[rump kernels](https://github.com/rumpkernel/wiki/wiki)** to
Akuma. A rump kernel is a real, unmodified NetBSD kernel — its drivers, file
systems, and TCP/IP stack — compiled to run as an ordinary program on top of a
small "hypercall" layer (`rumpuser`) instead of on bare hardware. Because the
hypercall layer only needs a handful of POSIX-ish primitives (memory, threads,
locks, clocks, and raw packet I/O), a rump kernel can be hosted on any system
that can provide them — including Akuma.

## Why

Akuma already has its own in-kernel TCP/IP stack (`crates/akuma-net`). The point
of rump is **not** to replace it. The point is to be able to run a **mature,
battle-tested NetBSD network stack** as an isolated userspace component, so that:

- Akuma gains a second, independent networking path that can be A/B'd against
  the native stack.
- We can host NetBSD drivers and protocols that Akuma does not implement
  natively, without writing them from scratch.
- Each network stack instance lives inside its own `box` (Akuma's container
  primitive), giving us isolation and a clean lifecycle (open / close / ps).

This is groundwork for the larger Akuma cluster vision: independent VMs and
boxes that each own a slice of networking and can be composed.

## Stated goals

1. **Port `rumpuser` to Akuma — in Rust.** Implement the rump hypercall
   interface as a **Rust crate that exports the `rumpuser_*` C ABI** (backed by
   `libakuma`), rather than cross-building NetBSD's C `librumpuser`. The rump
   kernel is C and links these symbols by name; the implementation being Rust is
   invisible to it. NetBSD's `librump*` static libraries then link and run as an
   Akuma userspace program.

2. **Run a NetBSD network stack in userspace.** Build a small program that boots
   a rump kernel with the NetBSD TCP/IP stack (`librumpnet` + the inet
   components), configures an interface, and serves traffic.

3. **Host it inside a box.** Add a `--net`-style switch to `box` (`userspace/box`)
   that creates a new box whose payload is the rump network stack instead of a
   generic command.

4. **Gate it behind a kernel feature.** Rump support requires a kernel-side raw
   packet path so the rump stack can send and receive Ethernet frames. That path
   is compiled in only when the kernel is built with a `rump` Cargo feature; the
   default build is unaffected.

5. **First end-to-end milestone:** a rump box that **bootstraps its address from
   DHCP** and then **`curl`s the QEMU host IP** successfully. Standard DNS
   resolution is a later milestone — DHCP init plus a successful HTTP fetch of a
   host IP is the bar for "it works".

## Layout

```
userspace/rumpkernel/
  README.md                     this file
  docs/
    IMPLEMENTATION_PLAN.md      the phased build/port plan (review target)
  buildrump.sh/                 git submodule: rumpkernel/buildrump.sh
                                (cross-builds the NetBSD librump* static libs)
```

`buildrump.sh` is the upstream tool that downloads the relevant subset of the
NetBSD source tree and cross-builds the rump kernel libraries for a target
toolchain. Akuma's target is `aarch64-linux-musl` static (see the plan).

## Status

**Resuming work? Start at [docs/HANDOFF.md](docs/HANDOFF.md)** — current state,
copy-paste reproduce steps, the next task, and gotchas.

In progress. The **kernel side** is built and verified; the userspace rump
libraries (cross-built `librump*`, the Rust `rumpuser`, the `virtif` backend, the
`rump-net` box payload) are not started yet.

- ✅ **Kernel prerequisite — `/dev/zero`** — see [docs/DEV_ZERO.md](docs/DEV_ZERO.md).
- ✅ **Phase 3 — kernel `rump` feature** (raw L2 `/dev/net/tap0` packet device on a
  dedicated second NIC, release-only, omitted from constrained profiles) — see
  [docs/PHASE3_KERNEL_TAP.md](docs/PHASE3_KERNEL_TAP.md).
- ✅ **Phases 0/1 — `librump*.a` built for aarch64-musl** (full NetBSD TCP/IP
  stack), via a Linux container — see [docs/PHASE01_BUILDRUMP.md](docs/PHASE01_BUILDRUMP.md).
- ✅ **Phase 2 — Rust `rumpuser`**: a full NetBSD rump kernel **boots on our
  hypercalls** — `rump_init()` returns 0 — see [docs/PHASE2_RUMPUSER.md](docs/PHASE2_RUMPUSER.md).
- ⏳ Phases 4/5/6 (virtif up + DHCP + `rump_sys_socket` in the container test → our
  `rumpcomp_user` backend to `/dev/net/tap0` → Akuma integration → box `--net` →
  DHCP + curl = M1). See [docs/IMPLEMENTATION_PLAN.md](docs/IMPLEMENTATION_PLAN.md).

Run the kernel tap path with `RUMP_NIC=1 cargo run --release` (adds the second
QEMU NIC). Without it, `/dev/net/tap0` is absent and the default boot is
unchanged.

## License

BSD. Userspace components under different licenses (GPL2, LGPL2) follow their respective licenses.

This code links with NetBSD project code, which was also used as reference. The NetBSD copyright belongs to NetBSD contributors.
