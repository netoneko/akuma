# Rump-kernel port — session handoff

**Read this first to resume.** Single source of truth for picking the work back
up. Detail docs: [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) (the full
plan + §10 forward architecture), [PHASE01_BUILDRUMP.md](PHASE01_BUILDRUMP.md),
[PHASE2_RUMPUSER.md](PHASE2_RUMPUSER.md), [PHASE3_KERNEL_TAP.md](PHASE3_KERNEL_TAP.md),
[DEV_ZERO.md](DEV_ZERO.md).
Post-M1 direction docs: [RUMP_SYSPROXY.md](RUMP_SYSPROXY.md) (**the committed next
architecture** — per-box rump server), [RUMP_PLUS_HERD.md](../../../docs/RUMP_PLUS_HERD.md),
[ARCHITECTURE_QUESTIONS.md](ARCHITECTURE_QUESTIONS.md) (unmodified-binary paths),
[FRANKENLIBC_EVAL.md](FRANKENLIBC_EVAL.md) (parked). Demos:
`acceptance/11_netbsd_rumpkernel_irc.md` (ssh-in + IRC), `acceptance/12_netbsd_binary_compatibility.md`.

Goal: **M1 DONE** (2026-06-22 — NetBSD stack in an Akuma box, DHCP + HTTP to the host).
**🏆 M2 DONE (2026-06-23) — the SHARED-STACK box via kernel-as-sysproxy-client:**
unmodified static binaries in a `stack=rump` box have their AF_INET routed by the
KERNEL to a shared boxed `rump_server`, validated end-to-end with **`curl` (HTTPS-by-IP)
AND `sic` holding a live IRC session on `#rumpkernel` (OFTC) over the NetBSD stack**.
See "M2 ACHIEVED" below + RUMP_SYSPROXY.md ("Phase B" / IRC). **DNS now works over
the rump stack (2026-06-23)** — `curl http://example.com` resolves + fetches HTTP 200
through NetBSD (`bind` + `sendto`-with-dest + `recvmsg` marshaling added to
`src/rump_proxy.rs`). Current target: drive down per-syscall latency (the one real
weakness) + robustness (task #9) + boot self-tests.

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
| **Unmodified `curl` does HTTP over the rump stack** (container, real round-trip) | ✅ **2026-06-22** (`docker-hijack-demo.sh`) |
| DHCP over the rump stack (container, vs dnsmasq) | ✅ **2026-06-22** (`docker-hijack-demo.sh` RUMP_DHCP=1) |
| Akuma backend: `rumpcomp_user` over `/dev/net/tap0` (vs container TUN/TAP) | ✅ `rumpuser/rumpcomp_tap.c` |
| Kernel: **blocking `read()`** on `/dev/net/tap0` (`Tap{nonblock}`, no busy-wait) | ✅ `read_frame_blocking`; self-test updated |
| 🏆 **M1 — DHCP + HTTP to the host, rump in an Akuma box** | ✅ **DONE 2026-06-22** (`rumphttp` in a `RUMP_NIC=1` box) |
| **Inbound TCP server over rump, reachable from the host** | ✅ **2026-06-22** (`rumpserver.c`; host `:2223`→rump `:22`, banner+echo) |
| **🏆 M2 — kernel-as-sysproxy-client: unmodified binary's AF_INET → shared boxed rump_server** | ✅ **DONE 2026-06-23** |
| **`curl` HTTPS-by-IP over rump** (`-H Host:ifconfig.me http://34.160.111.145` → `87.71.13.205`) | ✅ **2026-06-23** |
| **`sic` IRC: live `#rumpkernel` session on OFTC over rump** (acceptance/11 capstone) | ✅ **2026-06-23** (`163.61.26.35:6667`) |
| **DNS over rump** — `curl http://example.com` resolves + fetches via NetBSD (`bind`+`sendto`-dest+`recvmsg`) | ✅ **2026-06-23** (`example.com`→`104.20.23.154`→HTTP 200) |
| Phase 5 — herd autostarts `rumpnet` box (`--net --fd 3`, kernel attaches sysproxy channel) | ✅ **2026-06-23** (herd OWNS the rump_server; `restart=false`) |
| Rump SDK tarball (`bootstrap/archives/rump-sdk-aarch64-musl.tar.gz` → VM `/archives`) | ✅ `package-sdk.sh` (48 MB, 154 archives) |
| Akuma integration (libakuma, build-std core) | ✅ proven sufficient — stock host link runs on Akuma as-is |
| ⚠️ Per-syscall latency (~1s round-trip; rump pthread kthreads on 1 core) | ⏳ open — see "M2" + RUMP_SYSPROXY.md |
| ⚠️ Robustness: uninterruptible proxy syscalls, `kill` invalid-pid, client-slot wedge | ⏳ open — project task #9 |
| acceptance/11 — actual sshd on the rump stack | ⏳ (sic capstone met; sshd is the bigger protocol layer) |
| NetBSD binary compat (pkgsrc) via per-process syscall table | 📋 future — `acceptance/12_netbsd_binary_compatibility.md` |

Branch `netbsd-rump-kernel-attempt-0`. **The M2 kernel/herd/sic changes are UNCOMMITTED** (the
user commits): kernel (`src/rump_proxy.rs`, `src/syscall/{proc,mod,poll}.rs`,
`crates/akuma-rump/src/{sysproxy,syscall_translation}.rs`, `src/syscall/net.rs`,
`crates/akuma-exec/.../types.rs` `RumpSocket`, the gated scheduler tweak in
`threading/mod.rs`), herd (`rumpnet.conf` + `restart` flag), and the `sic` submodule
(`userspace/rumpkernel/sic` recv-drain patch, uncommitted in the submodule).

### 🏆 M1 ACHIEVED (2026-06-22): NetBSD stack in an Akuma box — DHCP + HTTP to the host

`rumphttp` (a static Akuma binary = rump TCP/IP + our rumpuser + the
`/dev/net/tap0` backend) ran in a `MEMORY=1024M RUMP_NIC=1` box and fetched a page
off the **Mac host** through the NetBSD stack:

```
NetBSD 7.99.34 (RUMP-ROAST)
virt0: Ethernet address b2:0a:38:0b:0e:00
dhcp: virt0: adding IP address 10.0.2.15/24        ← DHCP from QEMU net1 SLIRP
dhcp: virt0: adding default route via 10.0.2.2
RUMPHTTP: connect 10.0.2.2:8888 -> 0               ← TCP to the host
HTTP/1.0 200 OK ... <html><body>HELLO-FROM-MAC-HOST-VIA-RUMP</body></html>
[VIRTIF STATS] tx=78 pkts/5828 bytes  rx=9 pkts/1832 bytes (over /dev/net/tap0)
RUMPHTTP: PASS — fetched 240 bytes over the NetBSD rump stack (DHCP + TCP via /dev/net/tap0)
```

That is the M1 goal verbatim: NetBSD TCP/IP as a userspace rump kernel inside an
Akuma box, DHCP an address, HTTP the QEMU host through it.

**Reproduce:** `( cd userspace/rumpkernel && ./build-rumphttp.sh )` → copy
`/tmp/rumphttp_akuma` to `disk.img:/bin/rumphttp` (docker `--privileged` loop-mount)
→ run a host HTTP server (`python3 -m http.server 8888 --bind 127.0.0.1`) →
`MEMORY=1024M RUMP_NIC=1 cargo run --release` → over SSH (`:2222`):
`/bin/rumphttp 10.0.2.2 8888`. (The host is reachable at the SLIRP gateway 10.0.2.2;
QEMU's net1 SLIRP also serves the DHCP lease.)

**The `/dev/net/tap0` backend is blocking, not busy-wait:** the kernel tap `read()`
now blocks (`FileDescriptor::Tap { nonblock }`; `rump_tap::read_frame_blocking`
cooperatively yields like socket `recv`/`wait_until`, since Akuma's net is poll-based
with no RX IRQ). The RX thread in `rumpcomp_tap.c` does a plain blocking `read()`.
Boot self-test `test_rump_tap` updated to open `O_NONBLOCK` (still checks EAGAIN).

New files: `rumpuser/rumpcomp_tap.c`, `rumpuser/rumphttp.c`, `build-rumphttp.sh`;
kernel: `Tap{nonblock}` + `read_frame_blocking`.

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

### 🏁 Milestone (2026-06-22): UNMODIFIED `curl` does HTTP over the rump stack

`docker-hijack-demo.sh` runs an off-the-shelf `curl 8.14.1` (no recompile) so its
network syscalls hit the NetBSD rump stack instead of the host kernel, and proves
it with instrumentation:

```
[VIRTIF TX#1] ARP →  RX#1/2 ARP reply →  TX#3 SYN → RX#3 SYN-ACK → TX#4 ACK
[VIRTIF TX#5] 138 (HTTP GET) →  RX#5/6 (HTTP 200 + body) →  TX/RX teardown
[VIRTIF STATS] tx=7 pkts/498 bytes  rx=7 pkts/711 bytes
<html><body>HELLO-FROM-NETBSD-RUMP-STACK</body></html>   ← body returned to curl
```

How: `rumpuser/hijack.c` is a single `LD_PRELOAD` `.so` that statically embeds the
whole rump stack (PIC archives) + our rumpuser; a constructor `rump_init`s and
brings up `virt0`; libc `socket/connect/send/recv/readv/writev/poll/fcntl/
getsockopt` are interposed onto `rump_sys_*` (fd offset `0x40000000`; Linux→NetBSD
sockaddr + `SOCK_NONBLOCK`/`O_NONBLOCK` handling). `rumpuser/virtif_user_instr.c` is
the stock TUN/TAP backend + per-frame counters/log at the rump↔wire seam (the
proof). Container `tun0`=10.0.0.1/24 + a python http server stand in for the wire.

Hard-won lessons (all in `docs/ARCHITECTURE_QUESTIONS.md`):
- **`busybox wget` can't be hijacked via LD_PRELOAD on musl** — it uses `FILE*`
  (`fdopen`) and musl stdio flushes via *inline* `writev`/`readv` syscalls that
  bypass the PLT. Use curl/nc-class tools (direct `send`/`recv`) — or kernel-routing.
- curl uses Linux-only `SOCK_NONBLOCK|SOCK_CLOEXEC` type bits NetBSD rejects → strip
  them; keep the rump socket blocking so connect/send/recv stay synchronous.

Scope: proven **in the container**. An Akuma box swaps the TUN/TAP backend for our
`rumpcomp_user` over `/dev/net/tap0` and runs it as a herd service — same shim, same
proof. New files: `rumpuser/hijack.c`, `rumpuser/virtif_user_instr.c`,
`docker-hijack-demo.sh`; the virtif PIC archive (build via `docker-build-virtif.sh`
without `MKPIC=no`), and rumpuser built `-C relocation-model=pic`.

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
4. **Port the C glue → no_std Rust** (cleanup, do after it all works). `hijack.c`,
   `rumpcomp_tap.c`, and `rumphttp.c` are C for fast iteration, but none *need* to
   be — mirror `rumpuser/src/lib.rs` (no_std Rust exporting the C ABI). Interposers →
   `#[no_mangle] extern "C"`; the LD_PRELOAD constructor → `#[used]
   #[link_section=".init_array"]`; `rump_sys_*`/`dlsym(RTLD_NEXT)` → `extern "C"`
   decls. Only wrinkles: C-variadic `fcntl`/`open` (interpose with a fixed 3-arg
   signature — the optional arg sits in `x2` on aarch64; avoids nightly `c_variadic`),
   and the `.init_array` constructor. `csupport.c`'s variadic `rumpuser_dprintf` is
   the one piece that stays C-ish (variadic *definition* needs nightly — same reason
   it was split from the Rust rumpuser). The C files are the reference to debug
   against; keep them until the Rust port is proven equivalent.
5. **`/dev/net/tap0` should reset on close** (revealed by `rumpserver` testing). Only
   one rump process works per boot: an unclean exit leaves NIC1's RX two-phase state
   machine mid-flight, so the next `open("/dev/net/tap0")` can't receive (DHCP times
   out). Fine for a single long-lived box payload (sshd), but `close()`/process
   teardown should reset the `TapNic` RX state so a box can be restarted in place.

---

## 🏆 M2 ACHIEVED (2026-06-23): kernel-as-sysproxy-client — curl + IRC over rump

Sysproxy Steps 1–3 (spike / rump_server payload / rumpclient sharing) and the
transport-shape proof were already done; **M2 finished the kernel-as-client and
validated it end-to-end with two real unmodified binaries.** Architecture (decided
this session): **herd OWNS the one `rump_server` process** (`rumpnet.conf`:
`command=/bin/rump_server`, `args=--net --fd 3`, `stack=rump`, `restart=false`); herd
calls `SET_BOX_STACK` (syscall 324) before spawning, and when `sys_spawn_ext` sees that
spawn it calls `rump_proxy::attach_server`, which installs the kernel pipe pair on the
server's fd 3 (before it runs) and handshakes IN A KTHREAD. Then `handle_syscall` →
`rump_proxy::intercept_box_syscall` forwards a `stack=rump` box process's socket-family
syscalls (+ read/write/readv/writev/close on a `RumpSocket` fd) over the channel via
`akuma_rump::sysproxy::Client`, marshaled by `syscall_translation` + `ProcMem` (user-VA
copyin/copyout + sockaddr Linux↔NetBSD). New fd type `FileDescriptor::RumpSocket`.
Driving is **synchronous on the calling thread** (approach 1 — copyin/copyout hit
`current` VA); the kthread is only setup/handshake.

**Validated:**
- `box use rumpnet -i /bin/curl -sS -H Host:ifconfig.me http://34.160.111.145` →
  `87.71.13.205` over the rump stack (TCP path: socket/connect/getsockname/getsockopt
  /setsockopt/sendto/recvfrom).
- `box use rumpnet -i /bin/sic -h 163.61.26.35 -p 6667 -n netoneko` → full IRC
  registration + **live `#rumpkernel` session on OFTC** (acceptance/11 capstone).
  Required `readv`/`writev` marshaling (sic uses stdio) + `poll`/`select`-on-RumpSocket
  (MSG_PEEK probe) + a `sic` recv-drain patch (vendored: `userspace/rumpkernel/sic`).

## NEXT TASK — drive down latency + robustness (M2 weaknesses). Plan: RUMP_SYSPROXY.md

The path WORKS; the weaknesses are performance + robustness, not correctness:

1. **Latency (~1s per proxied syscall round-trip)** — root cause: the rump kernel's
   ~19 **pthread** kthreads contend on a single core (each round-trip waits on the
   scheduler; PSTATS shows them blocked in futex, woken by the 100Hz heartbeat). Tried:
   a scheduler wakeup-locality hint (no help, GATED OFF in `threading/mod.rs`
   `WAKEUP_LOCALITY_HINT=false`). **Fiber backend is BLOCKED** — `rumpfiber_sp.c` stubs
   out the sysproxy server (`abort()`), so fiber needs a from-scratch sp-server port.
   Untried lever: lower the rump kernel `hz` (set the `hz` global from `rump_server.c`
   BEFORE `rump_init` — `thetick` in `intr.c:doclock` is computed once at thread start;
   nothing reassigns `hz`) to cut heartbeat churn. (The sic recv-drain patch sidestepped
   it for sic by reading bursts in ~1 round-trip instead of per-byte.)
2. **Robustness (project task #9):** box procs stuck in a proxied syscall are
   UNINTERRUPTIBLE (the proxy channel read never checks `is_current_interrupted`/pending
   signals), `kill <pid>` of a box proc returns "invalid pid" (box/pid-namespace), and
   the single serialized `BoxProxy.client` slot means one wedged proc blocks ALL others
   (they spin in `with_client`). Only reboot clears it. Fix: interrupt/timeout the proxy
   read (return EINTR), reclaim the client slot from a dead holder, fix box-pid kill.
3. **DNS path (UDP): ✅ DONE (2026-06-23).** musl's resolver does
   `socket(AF_INET,DGRAM|NONBLOCK)` → `bind(INADDR_ANY:0)` → `sendto(query, ns:53)` →
   `recvmsg(answer)`. All three new calls are in `src/rump_proxy.rs`: `proxy_bind`
   (translate sockaddr like connect); `proxy_transfer` now marshals `sendto`'s dest
   addr (args[4]≠0 ⇒ UDP) and `recvfrom`'s source capture + `MSG_DONTWAIT`-when-
   nonblock so the drain loop ends on EAGAIN; and `proxy_recvmsg` decomposes the
   Linux `msghdr` in-kernel and drives the proven rump `recvfrom` (first iovec +
   `msg_name` source capture, `fromlenaddr`=the msghdr's `msg_namelen` field) — no
   full msghdr ABI translation needed. Validated live: `box use rumpnet -i /bin/curl
   http://example.com` → resolve `104.20.23.154` → HTTP 200. NOTE: requires a working
   nameserver — QEMU SLIRP's `10.0.2.3` returns empty answers on some hosts, so
   `bootstrap/etc/resolv.conf` now defaults to `8.8.8.8`/`1.1.1.1` (reached via the
   rump default route → SLIRP → internet). `sendmsg` (the send-side mirror) is still
   `EOPNOTSUPP` — this resolver sends via `sendto`, so DNS doesn't need it; add it if
   a glibc/c-ares client shows up. Multi-iovec `recvmsg` scatter is unimplemented
   (DNS is single-iovec; logged if `iovlen>1` ever appears).
4. **Kernel boot self-tests** for the proxy path (project policy — task #6).
5. **Security/hardening** (RUMP_SYSPROXY.md): sp-wire bounds-checks, seal `rumpuser__hyp`
   (mprotect), per-box isolation self-tests, channel-fd private to rump_server.
6. **Cleanup:** the `[RUMP-SP]` connect/transfer/iov debug prints are still in (gate them);
   `/dev/net/tap0` reset-on-close (workaround #5) so a box restarts without reboot.

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
