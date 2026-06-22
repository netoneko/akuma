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
| Phase 2 — Rust `rumpuser`: `rump_init()` returns 0 | ✅ **green** |
| Phase 4 — virtif up + DHCP + `rump_sys_socket` (still in container) | ⏳ **next** |
| Phase 4b — our `rumpcomp_user` backend → `/dev/net/tap0` | ⏳ |
| Akuma integration (libakuma, build-std core) | ⏳ |
| Phase 5 — `box open --net` spawns rump-net payload | ⏳ |
| Phase 6 — DHCP + curl host IP = **M1** | ⏳ |

Nothing is committed. Branch `netbsd-rump-kernel-attempt-0`.

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
  `docker-rumpuser-test.sh` (link + run rump_init).
- `rumpuser/` — Rust **no_std** staticlib: `src/lib.rs` (59 `rumpuser_*` symbols),
  `csupport.c` (variadic `dprintf` + the libkern overrides + `rust_eh_personality`
  stub), `test_init.c` (calls `rump_init`), `Cargo.toml` (`rumpuser_debug` feature).
- `src-netbsd/` (git-ignored) — pinned NetBSD source; `rumpuser.h` is at
  `src-netbsd/sys/rump/include/rump/rumpuser.h` (`RUMPUSER_VERSION 17`).

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
3. **`rumpuser` clock/lock "wrap"**: `cv_wait`/`clock_sleep` don't call the
   hypervisor `hyp_schedule`/`unschedule` around blocking yet (fine single-CPU at
   init; revisit under real concurrency).

---

## NEXT TASK — virtif + DHCP + socket, in the container

Stay in the container test harness (no Akuma yet) to prove the TCP/IP path:

1. Build the networking factions so the program can link them:
   `librumpnet.a`, `librumpnet_netinet.a`, `librumpnet_config.a` are already in
   `obj/dest.stage/usr/lib/`. The **virtif** faction is NOT built yet — `-k` skips
   `evalplatform` so `RUMP_VIRTIF` stays `no` (see PHASE01_BUILDRUMP.md). Either
   force `RUMP_VIRTIF=yes` for the libvirtif build, or build `libvirtif`'s
   `if_virt.c` directly and link our own `rumpcomp_user`.
2. Extend `test_init.c` (or a new `test_net.c`) to do, after `rump_init()`:
   `rump_pub_netconfig_ifcreate("virt0")` → `ifsetlinkstr(...)` →
   `rump_pub_netconfig_dhcp_ipv4_oneshot("virt0")` → `rump_sys_socket/connect/...`.
   Reference: `buildrump.sh/tests/nettest_simple/`.
3. Backend: for the container test, the simplest packet path is a host TAP/socket;
   for Akuma it becomes our `rumpcomp_user` over `/dev/net/tap0` (the kernel side
   is already done). Decide whether to prove networking against a host tap in the
   container first, or jump to wiring `/dev/net/tap0`.

After that: Akuma integration (libakuma + build-std core), then Phase 5
(`box open --net` spawns the rump-net payload), then Phase 6 (DHCP + curl = M1).

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
