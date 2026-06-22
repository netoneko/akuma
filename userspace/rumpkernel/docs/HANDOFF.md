# Rump-kernel port — session handoff

**Read this first to resume.** Single source of truth for picking the work back
up. Detail docs: [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) (the full
plan + §10 forward architecture), [PHASE01_BUILDRUMP.md](PHASE01_BUILDRUMP.md),
[PHASE2_RUMPUSER.md](PHASE2_RUMPUSER.md), [PHASE3_KERNEL_TAP.md](PHASE3_KERNEL_TAP.md),
[DEV_ZERO.md](DEV_ZERO.md).

Goal (M1): run the NetBSD TCP/IP stack as a userspace rump kernel inside an Akuma
`box`, DHCP an address, and `curl` the QEMU host IP through it.

---

## TL;DR status

| Piece | Status |
|-------|--------|
| Kernel `/dev/zero` prereq | ✅ done, boot self-test passes |
| Phase 3 — kernel `rump` feature: `/dev/net/tap0` raw L2 dev on 2nd NIC (`RUMP_NIC=1`, release-only) | ✅ done, verified on boot |
| `crates/akuma-rump` — host-testable tap orchestration + 14 unit tests | ✅ done |
| Phases 0/1 — `librump*.a` for aarch64-musl (full TCP/IP stack) | ✅ built (Linux container) |
| Phase 2 — Rust `rumpuser`: `rump_init()` returns 0 | ✅ **green** (container) |
| **`rump_init()` runs ON AKUMA** — NetBSD rump kernel boots in the VM | ✅ **GREEN 2026-06-22** |
| `rumpuser_component_*` family (scheduler bridge for virtif backend) | ✅ done, in `rumpuser/src/lib.rs` |
| Phase 4 — `librumpnet_virtif.a` (kernel driver `if_virt.o`) built | ✅ via `docker-build-virtif.sh` |
| Phase 4 — **container networking GREEN**: `virt0` up, IP assigned, `rump_sys_socket` OK | ✅ **2026-06-22** (`docker-net-test.sh`) |
| Phase 4 — `rumpuser` scheduler-wrap under concurrency | ✅ fixed (cv/mutex/rwlock + **clock_sleep**) |
| Phase 4 — DHCP + actual packet round-trip (needs tap peer / DHCP server) | ⏳ next |
| Rump SDK tarball (`bootstrap/archives/rump-sdk-aarch64-musl.tar.gz` → VM `/archives`) for in-VM builds | ✅ `package-sdk.sh` (48 MB, 154 archives) |
| Capstone demo: clone+compile sic, IRC `#rumpkernel` over the NetBSD stack | 📋 `acceptance/11_netbsd_rumpkernel_irc.md` (target) |
| Phase 4b — our `rumpcomp_user` backend → `/dev/net/tap0` | ⏳ |
| Akuma integration (libakuma, build-std core) | ✅ proven sufficient — stock host link runs on Akuma as-is |
| Phase 5 — `box open --net` / herd service spawns rump-net payload | ⏳ |
| Phase 6 — DHCP + curl host IP = **M1** | ⏳ |

Nothing is committed. Branch `netbsd-rump-kernel-attempt-0`.

### 🏁 Milestone (2026-06-22): rump kernel boots on Akuma

`test_init` (the Phase-2 program) was linked with the **host** `aarch64-linux-musl-gcc`
(same toolchain as `userspace/build.sh` — the container was only ever needed to
*build* librump, not to link the final ELF), copied to `disk.img:/bin/test_init`,
and run in the VM (`MEMORY=1024M RUMP_NIC=0`, networking off). Output:

```
NetBSD 7.99.34 (RUMP-ROAST)
cpu0 at thinair0: rump virtual cpu
RUMPUSER-AKUMA: rump_init() returned 0
RUMPUSER-AKUMA: PASS — NetBSD rump kernel booted on our rumpuser
```

No crash. Proves the whole Phase-2 stack (Rust rumpuser, libkern overrides,
`rust_eh_personality` stub, static ELF) survives transplant onto Akuma's own
musl/pthread/mmap syscalls. **Akuma integration was a non-event** — the binary
that passes in the container is already an Akuma binary (same triple). The
`herd` route (run it as an OCI-bundle service; herd also wires the process VFS /
mounts for us) is now a trivial follow-up.

---

## Environment facts

- Host: macOS arm64 (Apple Silicon). Docker daemon must be running.
- An arm64 Alpine container is **musl-native on aarch64** = Akuma's target, so we
  build librump + run the rump_init test *natively* in-container (no cross).
- Cross toolchain on host: `aarch64-linux-musl-gcc` (Homebrew). Rust target
  `aarch64-unknown-linux-musl` is installed.
- Big build outputs are **git-ignored** and currently **exist on disk** (so you
  can run the rump_init test immediately). A clean clone must re-run checkout +
  `docker-build.sh` (each ~1 min + a 375 MB clone).

---

## Reproduce everything (copy-paste)

```sh
cd userspace/rumpkernel

# (once) fetch pinned NetBSD source → src-netbsd/  (~375 MB, git-ignored)
./build.sh checkout

# (once) build librump*.a for aarch64-musl → obj/dest.stage/usr/lib/  (Linux container)
./docker-build.sh

# build the Rust rumpuser staticlib (host; no link step) → rumpuser/target/.../librumpuser_akuma.a
( cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl )
#   add --features rumpuser_debug to trace every hypercall to stderr

# THE Phase-2 test: link librump.a + rumpuser + run rump_init() in the container
./docker-rumpuser-test.sh
# expect: "RUMPUSER-AKUMA: rump_init() returned 0  / PASS"
```

Kernel side (Phase 3, separate from the above):
```sh
RUMP_NIC=1 MEMORY=1024M cargo run --release      # adds NIC1 → /dev/net/tap0; boot prints
                                                 #   [rump] /dev/net/tap0 bound to NIC1 + [Test] rump_tap PASSED
cargo test -p akuma-rump --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)"   # 14 host tests
```
(Use `MEMORY=1024M` so an unrelated pre-existing `test_mmap_file_oom` boot test —
which needs a `/models` file larger than RAM — skips instead of panicking.)

---

## What's built, with file pointers

**Kernel (`rump` cargo feature, in `default` so release-only):**
- `crates/akuma-rump/src/lib.rs` — `RawNic` trait + `TapNic<N>` (RX two-phase
  state machine, bounds guard, TX) + `select_second_net_addr`; 14 host tests.
- `crates/akuma-net/src/rump_tap.rs` — `impl RawNic for VirtioRawNic` (real
  virtio-net NIC1), global instance, MMIO probe.
- `src/syscall/fs.rs` — `/dev/net/tap0` open/read/write/fstat; `src/syscall/term.rs`
  — `TUNSETIFF` no-op; `crates/akuma-exec/.../types.rs` — `FileDescriptor::Tap`.
- `src/main.rs` — `rump_tap::init(&mmio_addrs)` after net init.
- `src/process_tests.rs` — `test_rump_tap` (in `run_network_tests`) + `test_dev_zero`.
- `scripts/cargo_runner.sh` — `RUMP_NIC=1` adds NIC1 on `virtio-mmio-bus.4`.
- `/dev/zero`: `FileDescriptor::DevZero` mirrored across `fs.rs`/`proc.rs`.

**Userspace rump (`userspace/rumpkernel/`):**
- `build.sh` (checkout|build|host|clean), `docker-build.sh` (librump in Alpine),
  `docker-build-virtif.sh` (builds the one `-k`-skipped faction, `librumpnet_virtif.a`),
  `docker-rumpuser-test.sh` (link + run rump_init in container).
- `rumpuser/` — Rust **no_std** staticlib: `src/lib.rs` (`rumpuser_*` symbols +
  the `rumpuser_component_*` scheduler-bridge family added 2026-06-22),
  `csupport.c` (variadic `dprintf` + the libkern overrides + `rust_eh_personality`
  stub), `test_init.c` (calls `rump_init`), `Cargo.toml` (`rumpuser_debug` feature).
- `src-netbsd/` (git-ignored) — pinned NetBSD source; `rumpuser.h` is at
  `src-netbsd/sys/rump/include/rump/rumpuser.h` (`RUMPUSER_VERSION 17`).

**To rebuild + re-run the Akuma boot of `test_init`:**
```sh
( cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl )
aarch64-linux-musl-gcc -O2 -static -o /tmp/test_init_akuma \
  rumpuser/test_init.c rumpuser/csupport.c -I obj/dest.stage/usr/include \
  -Wl,--allow-multiple-definition -Wl,--whole-archive \
    -L obj/dest.stage/usr/lib -lrump \
    rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a \
  -Wl,--no-whole-archive -lpthread
# copy into disk.img:/bin/test_init  (docker run --privileged, mount -o loop)
# from repo root:  MEMORY=1024M RUMP_NIC=0 cargo run --release
# then over SSH (port 2222): run `/bin/test_init`  (bare path; the shell rejects
#   `VAR=val cmd` prefixes — RUMP_VERBOSE defaults ON anyway)
```

---

## Decisions locked in (don't relitigate)

- **`rumpuser` is ours, in Rust, no_std** (libc/pthread glue), replacing NetBSD's
  C librumpuser (buildrump `-k`).
- **virtif**: reuse rump's **kernel driver `if_virt.c`** (the NIC inside the
  NetBSD stack), but write **our own `rumpcomp_user` backend** over Akuma
  syscalls — NOT the stock Linux TUN/TAP backend. (So `/dev/net/tap0`'s
  `TUNSETIFF` no-op is now optional, not load-bearing.)
- **2nd dedicated NIC** (plan §4 option A) for L2 isolation; NIC0 stays smoltcp.
- `rump` is release-only (in `default`; size/extreme `--no-default-features` omit it).
- Forward architecture (post-M1, plan §10): config-driven per-box rump instances,
  host = box 0; optional later "NetBSD stack as a box's primary AF_INET stack".

---

## Carried workarounds (revisit before shipping)

1. **libkern byte-loop overrides** (`rumpuser/csupport.c`): rump's *optimized
   aarch64* `rumpns_{memset,memcpy,memmove,strlen,strcmp,strncmp}` run away in our
   environment, so we override them with trivial byte loops, linked with
   `-Wl,--allow-multiple-definition`. **Proper fix:** build `librump` with the
   generic C libkern routines (not the aarch64 asm); root-cause why the optimized
   ones run away (DC-ZVA / `DCZID_EL0` assumptions or how buildrump assembled them).
2. **`rust_eh_personality` no-op stub** (`csupport.c`): prebuilt Rust `core`
   references it under `panic=abort`. **Proper fix on Akuma:** rebuild core with
   nightly `-Z build-std` `-Cpanic=immediate-abort` (like Akuma's other userspace).
3. **`rumpuser` scheduler-wrap under concurrency** — ✅ **FIXED (2026-06-22).**
   The blocking `rumpuser` primitives now release the single rump CPU around the
   host blocking call (NetBSD's `rumpkern_unsched`/`rumpkern_sched` discipline):
   `mutex_enter`/`rw_enter` wrap **on contention** (trylock first; spin mutexes use
   the no-wrap path); `cv_wait`/`cv_timedwait` use `cv_unschedule`/`cv_reschedule`
   (with the spin-kmutex interlock special-case); and — the actual culprit —
   **`clock_sleep`** now unschedules around its `nanosleep`. The hardclock thread
   was holding the one rump CPU through every 10 ms tick, starving the main lwp
   parked in the scheduler slowpath (`cv_wait_nowrap`) — a classic lost-CPU-handoff
   ("missed delivery"). Found via a thread-ID-stamped, single-`write` trace
   (`--features rumpuser_debug`; lines no longer tear). Also note: `mutex_init` now
   stores the `RUMPUSER_MTX_SPIN|KMUTEX` flags (needed for the wrap decisions) and
   `cv_timedwait` now treats the timeout as RELATIVE+CLOCK_REALTIME (was absolute).

---

## NEXT TASK — virtif backend + DHCP + socket, in the container

Path #2: prove the TCP/IP path in the container (cheap to debug), then bring the
known-good binary back to Akuma+herd. Progress so far:

- ✅ `librumpnet_virtif.a` built (`./docker-build-virtif.sh`). It contains the
  kernel driver `if_virt.o` only and exports `rump_virtif_virt_deliverpkt` (RX).
  It has **4 undefined backend symbols** the link must satisfy:
  `rumpcomp_virt_{create,send,dying,destroy}` (these come from the backend, which
  `-k`/`RUMPKERN_ONLY` deliberately skips — `Makefile.rump:143`). `VIRTIF_BASE=virt`
  ⇒ ifname `"virt"`, so the interface to create is **`virt0`**.
- ✅ `rumpuser_component_*` provided by our Rust rumpuser (the backend calls these
  to step in/out of the rump scheduler).

Remaining:
1. **Provide the backend** `rumpcomp_virt_{create,send,dying,destroy}`. For the
   container proof, compile the **stock** `virtif_user.c` (Linux TUN/TAP) as a
   standalone user object (needs `linux-headers` for `<linux/if_tun.h>`, and
   `-DVIRTIF_BASE=virt -I.../libvirtif -I obj/dest.stage/usr/include`). For Akuma,
   swap in our own `rumpcomp_user.c` over `/dev/net/tap0` (kernel side already done).
2. **Write `test_net.c`** — after `rump_init()`: `rump_pub_netconfig_ifcreate("virt0")`
   → `rump_pub_netconfig_ifsetlinkstr("virt0", "<devnum/tap>")` →
   either `rump_pub_netconfig_ipv4_ifaddr_cidr` (static, simplest first) or
   `rump_pub_netconfig_dhcp_ipv4_oneshot("virt0")` → `rump_sys_socket/connect/...`.
   API is in `obj/dest.stage/usr/include/rump/netconfig.h` + `rump/rump_syscalls.h`.
   Reference: `buildrump.sh/tests/`.
3. **Run in container** with `--device /dev/net/tun --cap-add NET_ADMIN` (stock
   backend opens `/dev/net/tun` + `TUNSETIFF`). Link like `docker-rumpuser-test.sh`
   but add `-lrumpnet -lrumpnet_net -lrumpnet_netinet -lrumpnet_config` and the
   virtif lib + the backend object.

After networking is green in the container: Phase 5 (herd OCI-bundle service for
the rump-net payload — herd wires the VFS/mounts), then Phase 6 (DHCP + curl = M1).

---

## Gotchas learned (will save you time)

- **Serial/QEMU + rump logs contain control bytes** → use `grep -a` or grep finds
  nothing ("binary file matches").
- **Link the rump test `-static`** — the lib dir also has `librump.so`, which
  `-lrump` prefers → runtime "librump.so.0 not found".
- **`docker run` in background "completes" early** (false signal) — poll the log
  for your own `EXIT=` marker or watch `docker ps`, don't trust the task notice.
- **2016 NetBSD vs modern toolchain**: host-tool build needs `-fcommon`, trailing
  `-Wno-error` (after `"$@"` so it beats NetBSD's `-Werror`), and a `__BEGIN_DECLS`
  cdefs shim on musl — all in `docker-build.sh`'s gcc wrapper.
- **Debug a rump crash**: `--features rumpuser_debug` traces hypercalls; `apk add
  gdb` in the container, break at the call site to read real args.
- **NetBSD banner**: `rumpuser_getparam` defaults `RUMP_VERBOSE` ON (kept out of
  respect for the NetBSD attribution); an env `RUMP_VERBOSE` overrides; the
  `rump_quiet` cargo feature silences the default.
- **`libakuma` is awkwardly structured** and should be broken up (deferred) — keep
  in mind when adding the rump-net userspace binary.
