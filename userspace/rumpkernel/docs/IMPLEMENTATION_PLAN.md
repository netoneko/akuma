# Rump Kernel Compatibility for Akuma — Implementation Plan

Status: **draft for review**. Nothing below is built yet.

This plan describes how to bring a NetBSD **rump kernel** to Akuma, with the first
concrete target being a **NetBSD TCP/IP stack running in userspace inside an Akuma
`box`**, bootstrapping its address over DHCP and successfully `curl`-ing the QEMU
host's IP.

---

## 0. Terminology

- **Rump kernel** — an unmodified NetBSD kernel component set (drivers, file
  systems, TCP/IP stack) compiled to run on top of a thin host abstraction
  instead of bare hardware. See <http://rumpkernel.org>.
- **`rumpuser`** — the **hypercall layer**: the ~50 functions a host must provide
  for a rump kernel to run (memory, threads, mutexes/cvs, clocks, console,
  random, and — for networking — raw packet I/O). Lives in NetBSD's
  `lib/librumpuser`. **Porting rump to a new host == implementing `rumpuser`.**
- **`librump*`** — the static libraries `buildrump.sh` produces:
  `librump` (core), `librumpuser` (hypercalls), `librumpnet*` (networking),
  `librumpvfs`, `librumpdev`, plus per-feature factions (`librumpnet_netinet`,
  `librumpnet_virtif`, etc.). An application links these and calls
  `rump_init()`.
- **`virtif`** — rump's "virtual interface": the rump NIC whose packets are
  driven by a host **packet-I/O hypercall**. On a Linux host this hypercall is
  backed by a **TUN/TAP** device (`/dev/net/tun`); on NetBSD by a real NIC.
- **`box`** — Akuma's container primitive (`userspace/box`). A box has a kernel
  `box_id`, a VFS root scope, and process membership, registered via
  `SYSCALL_REGISTER_BOX` (316) and torn down via `SYSCALL_KILL_BOX` (317).

---

## 1. End goal and success criteria

**Goal:** run the NetBSD network stack as an isolated userspace component on
Akuma, as groundwork for a multi-box / multi-VM Akuma cluster where networking
can be composed from independent stacks.

**Milestone M1 (this plan's bar for "it works"):**
1. `box open mynet --net` creates a new box whose payload is a rump-kernel
   program hosting the NetBSD TCP/IP stack.
2. Inside that box, the rump stack brings up a `virtif` interface, runs **DHCP**
   to obtain an address/route/MAC from the QEMU network, and
3. successfully performs an **HTTP `curl` of the QEMU host IP** (e.g.
   `http://10.0.2.2/`) *through the rump stack* (not Akuma's native smoltcp).

**Explicitly deferred (later milestones):**
- DNS resolution via the rump stack (M1 uses a literal host IP).
- Replacing or interoperating with Akuma's native smoltcp stack.
- File-system / device rump factions; only networking is in scope.

**Non-goals:** rebuilding the in-kernel `akuma-net` stack; rump is additive and
isolated.

---

## 2. How rump runs, and the one hard dependency

A rump network application is small (see
`buildrump.sh/tests/nettest_simple/nettest_simple.c`):

```c
rump_init();                                   /* boot the rump kernel */
rump_pub_netconfig_ifcreate("vif0");           /* create a virtif */
rump_pub_netconfig_ifsetlinkstr("vif0", ...);  /* bind it to the host packet I/O */
rump_pub_netconfig_dhcp_ipv4_oneshot("vif0");  /* DHCP (NetBSD provides this) */
fd = rump_sys_socket(AF_INET, SOCK_STREAM, 0); /* rump's own syscalls */
rump_sys_connect(fd, ...);                     /* talk via the rump stack */
```

Everything above runs **in-process** against `librump*`. The only thing that
must reach outside the process is the **packet-I/O hypercall** behind `virtif`:
the bytes the NetBSD stack wants to put on / take off the wire.

So the port reduces to two pieces of new code:
- **(a) `rumpuser` for Akuma — written in Rust.** Rather than cross-build
  NetBSD's C `lib/librumpuser`, we implement our own `rumpuser` as a **Rust crate
  that exports the `rumpuser_*` C ABI** the rump kernel calls (a `staticlib`
  exposing `#[no_mangle] pub extern "C"` symbols), backed by `libakuma`'s
  syscall wrappers. The rump kernel is C and links against these symbols by name;
  it neither knows nor cares that the implementation is Rust. This keeps the only
  hand-written host code in Akuma's native language and reuses the existing
  `libakuma` syscall surface instead of a parallel C layer.
- **(b) a `virtif` packet backend for Akuma** — raw L2 frame send/recv, also in
  Rust (the `rumpcomp_user_*` packet hypercalls exported as C ABI, talking to the
  kernel tap device). Same approach as (a).

**The `rumpuser` C ABI surface** (what the Rust crate must export, per NetBSD's
`rump/rumpuser.h`): initialization (`rumpuser_init`), parameters
(`rumpuser_getparam`), memory (`rumpuser_malloc`/`free`/`anonmmap`), threads
(`rumpuser_thread_create`/`join`/`exit`, curlwp), synchronization
(`rumpuser_mutex_*`, `rumpuser_rw_*`, `rumpuser_cv_*`), time
(`rumpuser_clock_gettime`/`clock_sleep`), randomness (`rumpuser_getrandom`),
console/diagnostics (`rumpuser_putchar`/`dprintf`), errno bridging
(`rumpuser_seterrno`), process control (`rumpuser_exit`/`kill`), and the
bi-directional scheduler hooks (`rumpuser_component_schedule`/`unschedule`,
used by the packet hypercall in `brlib/libtest/rumpcomp_user.c`). Pin the
`rumpuser` version constant to the one the pinned NetBSD source expects.

`buildrump.sh` already shows the template for both: for `*-linux*` targets it
sets `EXTRA_RUMPCOMMON='-ldl'`, `EXTRA_RUMPCLIENT='-lpthread'`, and enables
`RUMP_VIRTIF=yes` **iff `#include <linux/if_tun.h>` compiles** — i.e. the Linux
virtif backend is the TUN/TAP one (`buildrump.sh:1111`–`1124`,
`evalplatform()`). It also pulls a Linux syscall-emulation faction
(`sys/rump/kern/lib/libsys_linux`) for `evbearm`/x86 targets
(`buildrump.sh:786`–`801`). **Because Akuma binaries are `aarch64-linux-musl`
static ELF running on Akuma's Linux-compatible syscall ABI, we target rump's
existing `*-linux*` path rather than inventing a brand-new platform.** This is
the single most important strategic decision in this plan (see §4).

---

## 3. Akuma's current state (verified against the tree)

**Userspace C toolchain.** Every C member of `userspace/` is built with
`aarch64-linux-musl-gcc -static` (e.g. `userspace/build.sh:181`,`207`). Akuma
runs **static Linux/musl ELF** binaries via a Linux-compatible syscall layer.
This is what makes rump's `*-linux*` target viable.

**Box tooling.** `userspace/box/src/main.rs`:
- CLI flags are parsed by linear iteration over `libakuma::Args`; `box test
  --net` already threads a `--net` boolean (`cmd_test`, ~`main.rs:607`). Adding
  `box open --net` follows the same pattern in `cmd_open` (the existing
  `-i`/`-d`/`--root`/`-I` flag loop).
- A box is registered with `SYSCALL_REGISTER_BOX` (316) and processes are
  spawned into it with `SYSCALL_SPAWN_EXT` (315) via the `SpawnOptions { …,
  box_id }` struct; teardown is `SYSCALL_KILL_BOX` (317). Kernel side:
  `src/syscall/container.rs` and `src/syscall/proc.rs` (`sys_spawn_ext`),
  gated by the `sc-containers` Cargo feature.

**Networking — the gap.** Akuma's networking is **entirely kernel-owned**
(smoltcp). Userspace only sees a POSIX socket API (`src/syscall/net.rs`):
`SOCKET 198` accepts **only `AF_INET` (domain 2)** with `SOCK_STREAM`/`SOCK_DGRAM`;
`BIND/LISTEN/ACCEPT/CONNECT/SENDTO/RECVFROM/…` are 200–212; DNS is `RESOLVE_HOST 300`.
The smoltcp stack binds the virtio-net device at
`crates/akuma-net/src/smoltcp_net.rs:239` (`impl Device for VirtioSmoltcpDevice`,
with `VirtioRxToken`/`VirtioTxToken` at lines 294/307 — the exact L2 frame
boundary).

**There is no raw-packet path today:** no `AF_PACKET`, no TUN/TAP, no
`/dev/net/*`, no way for a userspace process to send/receive raw Ethernet
frames. **Creating that path is the kernel `rump` feature** (§6, Phase 3).

**Feature mechanism.** Kernel features live in the root `Cargo.toml [features]`
and are wired to `cfg(...)` via `build.rs` (e.g. `extreme` →
`CARGO_FEATURE_EXTREME` → `cfg(kernel_profile_extreme)`). Per-syscall families
are already gated this way (`sc-containers`, `sc-epoll`, …). The new `rump`
feature follows this exact pattern.

---

## 4. Architecture

```
            ┌─────────────────── Akuma kernel (cargo feature: "rump") ──────────────────┐
            │                                                                            │
 QEMU NIC0  │  virtio-net ──► VirtioSmoltcpDevice ──► smoltcp ──► AF_INET sockets        │  (native stack, unchanged)
            │                                                                            │
 QEMU NIC1  │  virtio-net ──► raw L2 tap path ──► /dev/net/tap0  (char device)           │  (NEW, only with feature "rump")
            └───────────────────────────────────────────────────┬────────────────────────┘
                                                                 │ read()/write() raw Ethernet frames
                                                                 │ (open/ioctl/read/write — TUN/TAP-shaped ABI)
   ┌─────────────────────── box "mynet"  (box open --net) ───────┴───────────────────────┐
   │  rump-net  (aarch64-linux-musl static ELF)                                           │
   │    rump_init()                                                                       │
   │    librumpnet_virtif  ──► rumpcomp_user packet hypercall ──► open("/dev/net/tap0")   │
   │    librumpnet_netinet (NetBSD TCP/IP)  ──► DHCP ──► rump_sys_socket/connect           │
   │    rumpuser  (RUST crate, exports rumpuser_* C ABI)  ──► libakuma syscalls            │
   │             (mmap, clone/futex, clock_gettime, read/write, getrandom, …)             │
   └──────────────────────────────────────────────────────────────────────────────────────┘
```

**Decision — dedicated NIC vs. shared NIC.** The rump stack needs L2 frames that
the kernel's smoltcp must *not* also consume, or the two stacks fight over the
same MAC/ARP/DHCP. Two options:

- **(A) Dedicated second virtio-net device (recommended).** Add a second NIC to
  the QEMU command line (`scripts/cargo_runner.sh`). The `rump` feature binds
  **NIC1** to the raw tap path and leaves **NIC0** on smoltcp. Clean isolation,
  matches the "independent network stacks" vision, no demux logic. QEMU user-net
  (`-netdev user`) gives DHCP + a host gateway (`10.0.2.2`) on each NIC for free,
  which is exactly what M1 needs.
- **(B) Shared NIC with a software bridge.** Tee frames at
  `VirtioSmoltcpDevice` and demux by destination MAC between smoltcp and the tap.
  More moving parts (promiscuous capture, MAC filtering); deferred unless a
  second NIC proves impractical.

**Recommendation: build M1 on (A).** It isolates the experiment and keeps the
native stack untouched.

**ABI shape of the tap device.** Implement `/dev/net/tap0` to mirror the slice of
the Linux TUN/TAP ABI rump's virtif uses: `open()`, the `TUNSETIFF`/`TAP` ioctl
(can be a no-op that returns success), and frame-granular `read()`/`write()`.
Matching that ABI lets us reuse rump's stock Linux virtif backend
(`sys/rump/net/lib/libvirtif/rumpcomp_user.c`) with minimal patching — the
single biggest lever for keeping NetBSD source changes near zero.

---

## 5. Toolchain & build strategy

Because `rumpuser` is ours (Rust, §2a), `buildrump.sh` is used to build the
**rump kernel** side only — the NetBSD source we do *not* rewrite — and our Rust
`rumpuser`/`virtif` libraries are substituted at the final link. buildrump's
`-k` mode ("only kernel (no POSIX hypercalls)", `buildrump.sh:71`) is the lever
for skipping NetBSD's C `librumpuser`; confirm in Phase 0/1 exactly which
faction libs `-k` emits and which we must still provide.

`buildrump.sh` cross-builds `librump*` given a working (cross) toolchain; it
downloads the relevant NetBSD source subset from
`github.com/rumpkernel/src-netbsd` (pinned by `.srcgitrev`
`82f3a690…`, `NBSRC_CVSDATE 20160728`). Build order is `librumpuser` →
`librump` → `librumpnet/vfs/dev` factions, installed as `.a` into `DESTDIR/usr/lib`
(`buildrump.sh:777`–`813`).

Plan of attack for the toolchain:
1. **Host sanity build first** (Phase 0) to learn the moving parts on a platform
   rump already supports.
2. **Cross-build for `aarch64-linux-musl`** (Phase 1). buildrump's `*-linux*`
   branch is the basis; `MACHINE_ARCH` for aarch64 maps to NetBSD `aarch64`.
   Expect to: point `CC`/`AS`/`LD`/`NM`/`AR` at the `aarch64-linux-musl-*`
   tools, pass `--host aarch64-linux-musl` so `librumpuser/configure` probes the
   cross target, and force `-static` everywhere (Akuma has no dynamic loader —
   cf. `akuma_own_tcc_build`: Akuma ELF must be `-static`).
3. **Vendoring.** Do **not** commit the multi-hundred-MB NetBSD checkout. Add a
   `userspace/rumpkernel/build.sh` that drives `buildrump.sh`/`checkout.sh` into
   an out-of-tree obj/dest dir, mirroring how other members script their builds.
   Pin the source rev via the submodule's `.srcgitrev`.

Open toolchain risks are tracked in §7.

---

## 6. Phased plan

Each phase has an explicit exit test. Kernel changes must add boot-suite
self-tests in `src/process_tests.rs` (per repo convention), not only e2e checks.

### Phase 0 — Host rump sanity (no Akuma)
- Run `buildrump.sh/buildrump.sh` natively on the dev host; run its bundled
  tests (`tests/testrump.sh`, `nettest_simple`).
- **Exit:** stock rump network test passes on the host. We now understand the
  build/test loop and the `librump*` artifact set.

### Phase 1 — Cross-build `librump*` for `aarch64-linux-musl`
- Write `userspace/rumpkernel/build.sh` invoking `buildrump.sh` with the
  `aarch64-linux-musl` cross toolchain, static-only, into
  `userspace/rumpkernel/obj/` + `dest/` (git-ignored).
- For this phase only, link against NetBSD's **stock C `librumpuser`** to prove
  the cross-build works end-to-end (our Rust `rumpuser` arrives in Phase 2). Build
  a trivial app linking `librump` + `librumpuser` that calls `rump_init()` and the
  demo hypercall `rumpcomp_user_testride()` (already in
  `brlib/libtest/rumpcomp_user.c`).
- **Exit:** a static `aarch64-linux-musl` ELF is produced and links cleanly. (It
  need not run on Akuma yet.)

### Phase 2 — `rumpuser` in Rust, running on Akuma
- Create a Rust crate `userspace/rumpkernel/rumpuser/` that builds as a
  `staticlib` and exports the `rumpuser_*` C ABI (§2a) via `#[no_mangle] pub
  extern "C"`, implemented on top of `libakuma`. Link it against the Phase-1
  rump kernel libs in place of NetBSD's C librumpuser.
- Implement/validate each hypercall family against Akuma's syscall surface:
  - memory: `mmap`/`munmap` (Akuma demand-pages anon mmap),
  - threads: `clone`/`futex` (Akuma has pthreads — cf. recent pthread fixes;
    `apk_fork_crash_extreme` shows thread-spawn works),
  - time: `clock_gettime`/nanosleep, console: `write`, random: `getrandom`/urandom.
- Where Akuma diverges from Linux/rump semantics, prefer fixing the Akuma syscall
  (and adding a kernel self-test) over working around it in Rust.
- **Kernel prerequisite — add `/dev/zero`.** `/dev/null` already exists and is
  complete (`src/syscall/fs.rs`: open `:985`, read→EOF `:483`/`:548`,
  write→discard `:583`/`:740`, stat as char dev `makedev(1,3)` `:1296`). `/dev/zero`
  is **not** implemented (no references in `src/` or `crates/`); some libc/rump
  anonymous-memory and buffer-zeroing paths expect it. Add a
  `FileDescriptor::DevZero` (`crates/akuma-exec/src/process/types.rs`, beside
  `DevNull`/`DevUrandom`) and mirror every `DevNull` branch in
  `src/syscall/fs.rs`, except: **read fills the buffer with zeros and returns
  `count`** (vs. `/dev/null`'s read→0), write discards and returns `count`, and
  stat reports `makedev(1, 5)`. Also list it in `src/vfs/proc.rs` (beside the
  `DevNull` arm). Add a `src/process_tests.rs` self-test (open `/dev/zero`, read
  N bytes, assert all-zero; write N bytes, assert returns N).
- **Exit:** with the Rust `rumpuser`, `rump_init()` returns success and
  `rumpcomp_user_testride()` prints from inside the rump kernel, on Akuma. Add a
  kernel self-test asserting the threads/futex/mmap paths rump exercised.

### Phase 3 — Kernel `rump` feature: raw L2 packet device
- Add `rump = []` to root `Cargo.toml [features]`; emit `cfg(feature = "rump")`
  (or a `cfg(kernel_rump)` via `build.rs`, matching the `extreme` pattern). Off
  by default so normal builds are byte-for-byte unchanged.
- Behind that feature:
  - Add a **second virtio-net device** (option A) and a `/dev/net/tap0` char
    device whose `read()`/`write()` move raw Ethernet frames to/from NIC1,
    bypassing smoltcp. Reuse the `VirtioRxToken`/`VirtioTxToken` plumbing in
    `crates/akuma-net/src/smoltcp_net.rs` as the reference for frame I/O, but
    route NIC1 to the tap queue instead of the smoltcp `Device`.
  - Implement the minimal TUN/TAP ioctl surface (`TUNSETIFF`/no-op) so rump's
    stock Linux virtif binds without source changes.
  - Update `scripts/cargo_runner.sh` to add NIC1 (under the feature / an env
    flag) with `-netdev user` so DHCP + host gateway exist.
- **Exit:** a tiny C test opens `/dev/net/tap0`, writes a crafted ARP/DHCP-discover
  frame, and reads the reply frame back. Add a `src/process_tests.rs` self-test
  for the tap device (loopback a frame).

### Phase 4 — Akuma `virtif` packet backend
- Build the rump networking factions (`librumpnet`, `librumpnet_netinet`,
  `librumpnet_virtif`) for `aarch64-linux-musl`.
- Ensure the virtif `rumpcomp_user` packet hypercall binds to `/dev/net/tap0`
  (ideally unmodified Linux backend; otherwise a thin Akuma `rumpcomp_user.c`
  under `userspace/rumpkernel/` doing `open/read/write` on the tap).
- **Exit:** a rump app creates a virtif, sends a frame, and the same frame is
  observed on the QEMU NIC (e.g. via host-side capture) — and vice-versa.

### Phase 5 — `rump-net` box payload + `box --net` switch
- New member `userspace/rump-net/` (or `userspace/rumpkernel/rump-net/`): the
  static ELF that `rump_init()`s, brings up the virtif, and exposes the M1
  behavior. Linked against the Phase-4 `librump*`.
- Add `--net` to `box open` in `userspace/box/src/main.rs` (mirror the existing
  flag loop). When set, `box open <name> --net`:
  - registers the box (316), and
  - spawns `rump-net` into it via `SPAWN_EXT` (315) as the box payload
    (instead of a user-supplied command).
- Keep the kernel-side box machinery as-is; `--net` is orchestration plus
  ensuring the box's process can open `/dev/net/tap0` (VFS scoping — the box root
  must expose `/dev/net`).
- **Exit:** `box open mynet --net` boots the rump stack inside the box; `box ps`
  / `box show` see it; `box close` tears it down cleanly.

### Phase 6 — DHCP bootstrap + curl host IP (**M1**)
- In `rump-net`: after virtif up, run NetBSD DHCP (rump exposes a oneshot DHCP
  configurator; `buildrump.sh/brlib/libnetconfig` carries the dhcpcd-derived
  client as a reference). Then issue an HTTP GET to the QEMU host IP
  (`10.0.2.2`) over the rump stack's sockets (`rump_sys_socket/connect/send/recv`).
- **Exit (M1 done):** from a cold `box open mynet --net`, the box logs a DHCP
  lease and prints the HTTP response body fetched from the host IP — proving the
  NetBSD stack carried real traffic on Akuma.

### Phase 7 — DNS (later)
- Configure the rump resolver from DHCP option 6 and switch the curl target from
  a literal IP to a hostname. Out of scope for M1.

---

## 7. Risks & open questions (call these out in review)

1. **Toolchain divergence.** rump's NetBSD build system expects a NetBSD-ish
   target; we're feeding it `aarch64-linux-musl`. The `*-linux*` path exists, but
   aarch64-musl-static may surface gaps (TLS model, `-static` + `-ldl`, missing
   `linux/if_tun.h` in the musl sysroot). Mitigation: Phase 0/1 de-risk before any
   Akuma work; we may ship a small `if_tun.h` shim.
2. **Source pin age.** The pinned NetBSD subset is from 2016. Fine for a stable
   TCP/IP stack; note it when chasing build breakage.
3. **`rumpuser` threading semantics.** rump assumes 1:1 pthreads with working
   futex and TLS. Recent Akuma pthread work is encouraging, but rump stresses it
   harder; budget time in Phase 2.
4. **One-NIC fights.** If we cannot add a 2nd virtio-net under QEMU HVF/our GIC
   setup, fall back to the software-bridge (option B) — more work.
5. **VFS device exposure in a box.** The box root must expose `/dev/net/tap0`;
   confirm `create_box_namespace` / VFS scoping lets a boxed process open it.
6. **Memory footprint.** A full rump kernel image + NetBSD TCP/IP is large
   relative to Akuma's tight RAM floors; M1 should target a roomy VM
   (≥256–512 MB), not the 4 MB extreme profile.
7. **Binary size / static link.** `librump*` static link may be big; confirm it
   loads under Akuma's ELF loader (use `read()`-based load, not file-backed mmap,
   per `llama_on_akuma_extreme`).

---

## 8. Files / directories to be created

```
userspace/rumpkernel/
  build.sh                 drives buildrump.sh/checkout.sh (cross aarch64-linux-musl, static, -k)
  obj/  dest/              build outputs (git-ignored)
  rumpuser/                RUST staticlib crate: exports rumpuser_* C ABI over libakuma (§2a)
  rumpcomp_akuma/          RUST virtif packet backend (rumpcomp_user_* → /dev/net/tap0)
  docs/IMPLEMENTATION_PLAN.md   (this file)

userspace/rump-net/        the rump network-stack box payload (rump_init + virtif + DHCP + curl)

src/ (always, kernel prerequisite — not feature-gated):
  /dev/zero char device    FileDescriptor::DevZero, mirrors /dev/null but read→zeros,
                           stat makedev(1,5) (crates/akuma-exec types.rs, src/syscall/fs.rs,
                           src/vfs/proc.rs); /dev/zero self-test in src/process_tests.rs

src/ (behind cargo feature "rump", off by default):
  dev/net tap char device  /dev/net/tap0 (open/ioctl/read/write raw frames)
  second virtio-net binding (NIC1 → tap queue, bypassing smoltcp)
  src/process_tests.rs     tap-device loopback self-test
build.rs                   emit cfg for the "rump" feature
Cargo.toml                 add `rump = []` to [features]
scripts/cargo_runner.sh    add NIC1 (-netdev user) under the feature/env flag
userspace/box/src/main.rs  `--net` switch on `box open` → spawn rump-net payload
```

---

## 9. Review checklist for the maintainer

- Confirm **`rumpuser` is implemented in Rust** as a `staticlib` exporting the
  `rumpuser_*` C ABI over `libakuma` (this plan's approach), with buildrump's
  `-k` mode used to avoid building NetBSD's C librumpuser.
- Agree on **dedicated 2nd NIC (A)** vs **software bridge (B)** for L2 isolation.
- Agree on the **tap device ABI** (TUN/TAP-shaped vs. a bespoke Akuma packet
  syscall). TUN/TAP-shaped maximizes reuse of stock rump virtif.
- Confirm `aarch64-linux-musl` static is an acceptable rump target (vs. defining
  a fresh rump platform), and that the 2016 source pin is fine.
- Confirm `rump` should be a kernel **cargo feature** (off by default), gating
  only the raw packet path — native networking untouched.
- Confirm M1 success bar: **DHCP lease + HTTP GET of the QEMU host IP through the
  rump stack**, DNS deferred.

---

## 10. Forward-looking architecture (post-M1, not required for M1)

These directions emerged in review. None are needed for M1, but they shape how
the pieces are factored now so we don't paint ourselves into a corner. The
**kernel side already factors with this in mind**: the host-testable orchestration
(NIC selection + RX state machine) lives in `crates/akuma-rump` behind a `RawNic`
trait, and `TapNic<N>` is an **instance** type, not a hard singleton — the current
single global in `akuma-net::rump_tap` is just the first wiring, not a ceiling.

### 10.1 Config-driven rump init, recursive over the box model
Today `rump_tap::init()` is called once at kernel boot for NIC1. The natural
generalization is a **config-driven init**: the kernel (feature-gated) brings up a
rump network instance from a declarative `RumpNetConfig` — which NIC/backend, MAC,
DHCP vs. static IP, routes. This maps cleanly onto Akuma's existing **nested box
model**: a box's spec carries its network-stack config, and the **host is just
box 0 with the root config**, running the *same* setup path as any child box. So
instead of one global tap, a registry keyed by `box_id` holds one rump instance
per box that asks for one; `box open <name> --net [config]` (Phase 5) becomes
"register box + attach a rump instance built from its config", and the host's own
stack is the box-0 case of the identical mechanism. `TapNic<N>` being per-instance
is what makes this a wiring change (a `BTreeMap<box_id, TapNic<…>>` + a config
struct), not a redesign. **Worth prototyping right after M1.**

### 10.2 NetBSD stack as a (per-box) primary stack, not just additive
M1 keeps rump strictly additive and isolated (NIC1), with smoltcp owning NIC0 and
the POSIX socket syscalls. A later option is to let a box (possibly box 0) use the
**NetBSD stack as its primary networking**, i.e. route that box's `socket`/`connect`/
`send`/`recv` syscalls to its rump instance instead of smoltcp. That needs new
**kernel wiring**: a per-box "which stack backs AF_INET" selector in
`src/syscall/net.rs`, translating the POSIX socket calls into `rump_sys_*` against
the box's rump instance (the userspace rump program already exposes `rump_sys_socket`
etc.; the kernel-side equivalent would proxy to it or host rump in-kernel). This is
a significant step (it touches the hot socket path and the smoltcp ownership model)
and is explicitly **out of scope for M1** — noted so the §10.1 config carries a
"stack = smoltcp | rump" knob from the start, leaving room to flip it per box later
without re-plumbing.

### 10.3 Per-instance resource constraints (at least memory)
Each rump instance is a full NetBSD kernel image (uvm, mbuf pools, socket buffers,
the TCP/IP stack) running inside an Akuma process. Left unbounded, a single rump
box can balloon its host process and starve the rest of the system — which on a
small-RAM Akuma (the 4–8 MB floors we tune for) is fatal, and on the cluster
(LLM box + agent box, see the cluster-vision memory) is the difference between
graceful backpressure and a kernel-wide OOM abort.

So the §10.1 per-box config should carry **resource constraints, memory first**:
- **Memory cap** per rump instance. Two layers: (a) tell the rump kernel itself
  how much it may use — NetBSD rump honours `RUMP_MEMLIMIT` (consulted in
  `uvm`/`pool` sizing), which our Rust `rumpuser` reads via `rumpuser_getparam`
  and can clamp/default from the box config; (b) bound the *host* process from
  Akuma's side (the box's PMM/heap budget) so a runaway rump can be SIGKILL'd and
  reclaimed rather than taking the kernel down — this dovetails with the existing
  `akuma_oom_kill_not_panic` / PMM-reserve work.
- **Later knobs** (same config surface, lower priority): CPU/scheduling share
  (rump is single-CPU per instance today, but instance count needs bounding),
  socket-buffer / mbuf-cluster limits, and an open-fd / packet-rate ceiling for
  the `/dev/net/tap0` backend so one box can't monopolise the NIC.

Design intent: the constraint lives in config from day one (like the
`stack = smoltcp | rump` knob), so multi-box rump is bounded-by-default instead of
retrofitting limits after the first OOM. The memory cap is the M-after-M1 must-have;
the rest can follow.

### 10.4 Running *unmodified* binaries against the rump stack
(Full discussion + the user Q&A that drove it: `docs/ARCHITECTURE_QUESTIONS.md`.)

`rumpuser` is the rump kernel's *downcall* layer to the host, **not** a syscall
interceptor — so a normal `busybox curl` calls libc `socket()` → a native trap to
**Akuma** (smoltcp), never the rump stack. The rump stack is reached only via
`rump_sys_*`. Three ways to bridge that, in increasing ambition:

1. **Link against rump** (`rump_sys_*`) — the M1 proof path (`sic`, acceptance 11).
   Purpose-built binary; no infra. Works today once the stack networks.
2. **Preload + hijack** — `LD_PRELOAD=librumphijack` intercepts libc socket calls →
   `rump_sys_*`, either in-process or proxied to a `rump_server` via the `sp_*`
   hypercalls. **Viable on Akuma** (Akuma *has* dynamic linking — apk ships dynamic
   musl ELFs; only our own tcc-built binaries are forced `-static`). Cost: build
   `librumphijack`/`librumpclient` and **un-stub the `sp_*` hypercalls** in our Rust
   rumpuser (today they return `ENOTSUP`).
3. **Kernel-side routing** (Akuma-native, generalizes §10.2) — Akuma owns the box's
   syscall entry, so it can forward the box's network (or, with **`libsys_linux`**,
   *all* Linux) syscalls to that box's rump instance with no preload. `libsys_linux`
   (the Linux→NetBSD syscall ABI table inside rump) is **not built for `evbarm64`
   yet** (buildrump's emul dir resolves empty for our MACHINE) — building it needs
   aarch64 portability work, and is the enabler for fully unmodified Linux binaries.

**herd must carry this config — and via OCI it largely already can.** herd parses
OCI `config.json` (`process.args`, `process.env`, `mounts`, `root_path`). The bits
this needs are all expressible there: `env` for `LD_PRELOAD=librumphijack` +
`RUMP_SERVER=unix://…` (model 2); `mounts` to bind `/dev/net/tap0`, the rump SDK,
and the server socket into the box namespace (herd already wires the process VFS).
What may need adding to herd: an explicit per-service **stack/networking selector**
(`smoltcp | rump`, model 3) and the resource caps from §10.3 — i.e. a small
Akuma-specific extension to the OCI bundle, not a new mechanism. Track this as a
herd requirement so the bundle schema reserves the knobs early.
