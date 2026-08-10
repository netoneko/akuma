# Rump stack (sysproxy + fiber backend)

> **Status: DEFERRED (2026-07-19).** `rump_server` work is on hold. The default
> devbox is now **devbox-smoltcp** (native smoltcp stack + real shared-kernel
> SMP) — see [`smp-shared.md`](smp-shared.md) and
> [`../../../overlays/devbox/README.md`](../../../overlays/devbox/README.md). The
> rump path below still builds and boots (`overlays/devbox/run.sh`); it is just
> no longer the recommended image. This doc describes it as last left.

> **Stability: C (active risk).** `src/rump_proxy.rs` and its supporting
> crates had 8+ commits in the week before this doc was last verified
> (2026-06-30 to 2026-07-06: fiber-by-default, tap-fd poll, sshd-over-rump,
> the multikernel/core2 variant). Treat any specific latency number here as a
> snapshot, not a guarantee.

Internals of the NetBSD rump TCP/IP stack as it runs inside Akuma: one real
NetBSD kernel, running as a userspace process, that other Akuma processes
share over a kernel-mediated remote-syscall protocol ("sysproxy"). For how a
box picks a stack and how packets flow at the box-routing level, see
[`networking.md`](networking.md).

## Components

| Component | Location | Role |
|---|---|---|
| `rump_server` binary | `userspace/rumpkernel/rumpuser/src/rump_server.rs` (feature `rump_server_main`) | The process that owns the NetBSD stack + `/dev/net/tap0`. ~14 MB, Rust `main` (ported from a C wrapper; NetBSD's C sysproxy server is still linked as-is). |
| Kernel sysproxy client | `src/rump_proxy.rs` (1,458 lines) | Forwards box processes' socket syscalls to the server over fd 3; owns per-box dispatch, isolation, and the `rump-default` box-0 bring-up. |
| Rump tap driver | `crates/akuma-net/src/rump_tap.rs`, `rumpcomp_tap.c` | Raw L2 frame path on NIC1 → `/dev/net/tap0`. |
| Sysproxy protocol client | `crates/akuma-rump/src/sysproxy.rs` | The rumpsp wire client (handshake + COPYIN/COPYOUT/ANONMMAP callback loop), generic over a `Transport` + `ClientMem`. Host-tested. |
| Syscall translation | `crates/akuma-rump/src/syscall_translation.rs` | Linux aarch64 sysno ↔ NetBSD sysno + `Op` enum; `sockaddr_in` Linux↔NetBSD; errno map; the socket-family isolation guard. Host-tested. |
| Fiber (cooperative) backend | `userspace/rumpkernel/rumpuser/src/fiber.rs` (~580 lines) | Rust port of NetBSD's `rumpfiber.c`: collapses rump's ~19 pthread kthreads onto **one OS thread**. **Default** (`threads_fiber` cargo feature, on by default; `--no-default-features` for the legacy pthread backend). |

## The sysproxy architecture (kernel-as-client)

**One `rump_server` process per rump box** owns the NetBSD stack + the tap.
Other in-box processes share it via rump's **sysproxy** (remote-syscall)
mechanism. **Akuma's kernel is the sysproxy client**: unmodified binaries'
`AF_INET`-family syscalls are intercepted in `handle_syscall` and forwarded to
the box's rump server over a kernel pipe pair on fd 3 — no `LD_PRELOAD`, no
per-binary linking, no `sp_*` client library in the app.

- Transport = kernel pipe pair (Akuma has no path-based `AF_UNIX`, only
  `socketpair`).
- The proxy is **synchronous on the calling thread**: each forwarded syscall
  blocks the caller until the server replies. Marshaling (`ProcMem`
  copyin/copyout) runs kernel-side against the *calling process's* VA — no
  cross-address-space walk.
- The server's own pid is recorded in `SERVER_PIDS` (before its handshake)
  and excluded from interception, so its own channel I/O hits the real
  NetBSD stack natively instead of looping back into itself.

Two ways a box ends up on rump — both use the same kernel machinery:

### 1. `rump-default` (box 0 — the devbox)

`rump_proxy::start_default_stack` (`src/rump_proxy.rs:1284`), compiled only
under the `rump-default` feature (part of the `devbox` meta-feature):

1. Bails if NIC1 isn't ready (`/dev/net/tap0` missing → box 0 stays native).
2. `mark_box_rump(0)`.
3. Spawns `/bin/rump_server --net --fd 3 --log /var/log/box/0/rump_server.log`.
4. `attach_server(0, pid)` — wires fd 3 + handshakes in a kthread (~5s:
   `rump_init` + DHCP over `/dev/net/tap0`), publishes the proxy. The server
   is **never** killed — it is box 0's live stack for the process lifetime.

`main` does not block on the handshake (the rumpsp fiber only advances while
the host scheduler keeps churning; herd's `start_delay_ms` + `restart` cover
the bring-up window). After this, every ordinary unboxed process — login
shell, sshd, curl, meow — has its socket syscalls transparently routed. No
herd box, no `box_root`, no `join_box`. See
[`networking.md`](networking.md) "How box 0 gets its stack" for the
box-routing side of this.

### 2. `stack = rump` herd box — **implemented and running**

A herd-owned `rump_server` in a **fresh box** (not box 0), on a
default-smoltcp build. herd's `ServiceConfig.stack` field (`"" | "smoltcp" |
"rump"`, `userspace/herd/src/main.rs:124`) drives `set_box_stack_rump`
(`main.rs:862`) before the service spawns; other services `join_box` into it
to share the same stack (e.g. `bootstrap/etc/herd/core2/sshd-rump.conf`:
`join_box = rumpnet`). Live examples on disk:
`bootstrap/etc/herd/available/rumpnet.conf` (box 0's peer path, boxed) and
`bootstrap/etc/herd/core2/rumpnet.conf` (the per-core variant, below). No
auto-restart yet (`restart = false`) — see "Known limitations".

This was tracked as "Phase 5 / partly implemented" in earlier planning docs;
it is done. See `archive/RUMP_PLUS_HERD.md` for the original design (frozen;
some details, like restart lifecycle, are still accurate as open items).

## The fiber backend (why rump works on one vCPU)

Out of the box, rump spawns ~19 pthread kthreads. On a single-vCPU guest
these contend for the rump kernel's single virtual CPU (`ncpu == 1`) and
re-wake each other on every 100 Hz heartbeat tick — a thundering-herd effect
that dominated early latency measurements (~0.8–4 s per proxied syscall).

**Investigated and ruled out:** lowering the rump heartbeat rate (`hz`) and
narrowing the CPU-release wakeup (`cv_broadcast` → `cv_signal`) were built,
measured, and made latency *worse* — the herd-heartbeat theory undersold the
real cost. See `archive/RUMP_LATENCY_SLEEP_FIX.md` for the full disproof; it
is preserved as a "don't re-try this" record.

**What actually fixed it:** the **cooperative-fiber** `rumpuser` backend
(`threads_fiber` feature, now default) collapses the ~19 OS threads into
**one OS thread** running them as fibers (a hand-rolled aarch64 context
switch — musl ships no `ucontext`/`swapcontext`). This required also porting
the sysproxy *server's* blocking primitives (`pthread_create`,
`pthread_mutex_*`, `pthread_cond_*` in NetBSD's `rumpuser_sp.c`) to
cooperative fiber shims at runtime (`sp_serve_fd.c` + `akfiber_sp_*` in
`fiber.rs`) — otherwise a blocking channel read on the one OS thread would
freeze every fiber, including the one that would produce the reply.

Result (single-core, box-0 rump-default; see `archive/FIBER_HANDOFF.md` for
the full history): `rump_server` OS thread count **19 → 1**, PSTATS
`clone=0 futex=0`, and `curl http://example.com/` **62.8 s → 16.3 s → ~1.4 s**
(the last step was a separate fix — honoring `O_NONBLOCK` on connected
`recvfrom` so curl's poll loop doesn't block on the proxy's transport
timeout, not a fiber change).

**Per-syscall / keystroke latency (2026-07-19).** The residual ~300 ms
single-keystroke floor over rump (vs ~38 ms on smoltcp) was root-caused to
**Akuma-side scheduler round-trip latency**, not rump_server compute: a proxied
syscall's request was written to the kernel→server pipe as two `pipe_write`s, and
the first wake-SGI preempted the box thread mid-request → the server woke on a
partial frame → the box waited ~5 × the 10 ms preemption tick to finish. Proof: an
EAGAIN `recvfrom` (server does no work) still cost ~48 ms. Fixed by **coalescing
the request AND the reply each into one `pipe_write`** (single wake, complete
frame): keystroke p50 **318 → 219 ms**. The remaining ~20 ms/round-trip floor is
rump_server's own internal fiber cadence; the wasted-EAGAIN-poll and `sendto`
copyin-callback round trips are the next levers (the EAGAIN one needs kernel push
readiness — see "Known limitations"). Full analysis + corrected earlier
mis-conclusion + reverted dead ends: `archive/RUMP_SYSPROXY_LATENCY_FIX.md`
Phase 3q.

**Rump tax vs native smoltcp — measured A/B (2026-07-19).** The same `curl`
(8.11.1 / mbedTLS), same `devbox.img`, same QEMU SLIRP + DNS, run minutes apart
against `http://example.com/`: box 0 on **native smoltcp** (default `--release`
build, in-kernel stack) vs box 0 on **rump** (devbox build, sysproxy). Medians:

| phase (cumulative) | smoltcp (in-kernel) | rump (sysproxy) | rump tax |
|---|---|---|---|
| DNS resolve | ~0.085 s | ~0.57 s | +0.48 s |
| + TCP connect | ~0.10 s | ~0.85 s | +0.27 s |
| + first byte | ~0.12 s | ~1.13 s | +0.24 s |
| **total (HTTP GET)** | **~0.13 s** | **~1.13 s** | **+1.0 s (~8.7×)** |
| **total (HTTPS GET)** | **~0.30 s** | **~1.9 s** | ~6× |

The key correction this settles: the ~1.1 s rump `curl` is **NOT** dominated by
external internet RTT. The identical GET over smoltcp — same DNS server, same
SLIRP path — completes in **~0.13 s**, so external latency is only ~0.13 s and the
remaining **~1.0 s is pure rump/sysproxy tax**. It is spread across *every* phase
(~0.24–0.48 s each) because each phase is several socket syscalls and each syscall
is a cross-process round-trip through the cooperatively-scheduled NetBSD kernel
(the `N × per-syscall-cost` amplification). Consistent with the keystroke figure
(rump ~225 ms vs smoltcp ~38 ms, ~6×): same per-syscall tax, different `N`.

**Implication:** rump is a NetBSD-stack correctness/compat vehicle, not a
low-latency one — it costs ~1 s on a trivial request, and no scheduler tuning
inside rump (tick, network-thread boost, RX pump) closes an ~8× gap; those shave
the rump portion only at the margin. For latency-sensitive networking, route the
hot path through **native smoltcp**; reserve rump for where NetBSD semantics are
genuinely required, and there prefer an in-process (no-sysproxy) model. See
`archive/HIJACK_VS_KERNEL_PROXY.md` for the in-process vs kernel-proxy trade.

## Syscall marshaling — what's proxied

`op_from_linux_sysno` (`crates/akuma-rump/src/syscall_translation.rs:49`)
marshals: `socket`, `bind`, `listen`, `accept`, `connect`, `getsockname`,
`getpeername`, `sendto`, `recvfrom`, `setsockopt`, `getsockopt`, `shutdown`,
`sendmsg`, `recvmsg`, `read`, `write`, `readv`, `writev`, `close` — enough
for curl, sic (IRC), and sshd-over-rump. **Not yet marshaled:** `accept4`,
`recvmmsg`, `sendmmsg`, `sendmsg`'s multi-iovec path.

**Hard isolation guarantee.** `is_socket_family_sysno`
(`syscall_translation.rs:96`) is a *superset* of the marshaled ops — it also
lists the socket syscalls not yet implemented, so `intercept_box_syscall`
can return a clean `EOPNOTSUPP` for them on a rump box instead of silently
falling through to native smoltcp. For a `stack=rump` box, **any
socket-family syscall (by number) or any syscall on a rump-owned fd is owned
by the proxy — routed if marshalable, `EOPNOTSUPP` otherwise, never native.**
`socketpair` (199) is the one deliberate exception: it's AF_UNIX-only, pure
local IPC (never networking), so it always runs natively even on a rump box
— required for Rust's `std::process::Command`, which uses a socketpair as
its exec-status channel for every subprocess spawn.

## Known limitations (current)

- **No supervised restart.** `restart = false` on every `stack=rump` herd
  service: if `rump_server` dies, the kernel's sysproxy channel + per-box
  proxy are orphaned, and a correct restart needs to re-establish both plus
  back off. Only clean recovery today is a VM reboot.
- **One serialized client slot per box.** A box proc stuck in a proxied
  syscall is uninterruptible (the channel read doesn't check pending
  signals), and the single `BoxProxy.client` slot means one wedged box proc
  blocks every other process sharing that box's stack.
- **No push readiness for rump sockets — and the idle probes are load-bearing.**
  Readiness is a `MSG_PEEK` sysproxy round-trip; `epoll_check_fd_readiness`
  (`src/syscall/poll.rs`) registers **no waker** for a `RumpSocket` (unlike
  `Stdin`), so poll re-probes every ~10 ms instead of blocking on an event, and
  the sshd bridge spin-`try_read`s the socket each loop iteration (~20 ms/probe)
  while waiting for the shell echo. This *looks* like pure waste but is not:
  **those idle probes pump rump_server's tap RX.** NIC1 has no RX IRQ, so
  rump only reads `/dev/net/tap0` when its cooperative fiber scheduler runs the
  RX fiber — and each proxied recvfrom wakes rump_server over the sysproxy pipe,
  which runs that fiber. A Phase 3a attempt to gate the probes behind a
  tap-frame push signal (skip the round-trip when no frame arrived recently)
  **stalled every interactive session**: with the probes gone, rump stopped
  reading the tap (`tap_rx` advanced +4 frames/30 s vs ~25 baseline), so
  readiness never advanced and every recv skipped forever. Measured with an
  `ssh -tt` PTY harness: baseline 8/8 @ ~225 ms, gated 0/10 (total stall).
  The **Phase 3a2 kernel RX pump** (timer IRQ → `has_frame` → wake the network
  thread) was then built and *does* cure the RX starvation (pump fires,
  `lockfail=0`, RX flows, some boots 10/10) — but it still isn't shippable: the
  recv gate stays **bistable/flaky across boots** (10/10 vs 0/10, same binary)
  because rump's frame→socket-buffer processing lag at a fixed generation has no
  safe recency bound, and the latency win is marginal anyway (~212 ms vs 225 ms
  baseline). Both attempts reverted. A real fix needs **event-accurate per-socket
  readiness** (rump signalling readiness back over a side channel, or an
  in-process/frankenlibc model that removes the sysproxy round-trip), not
  probe-gating a coarse global generation. Full write-up:
  `archive/RUMP_SYSPROXY_LATENCY_FIX.md` Phase 3a + 3a2.
- **One rump per `/dev/net/tap0` per boot.** The NIC1 RX two-phase state
  machine isn't reset on close, so only one rump owner can hold the tap per
  boot.
- **`csupport.c` byte-loop overrides.** The stock optimized aarch64
  `memset`/`memcpy`/`memmove`/`strlen`/`strcmp`/`strncmp` in `librump.a` run
  away in this build/link environment (root cause still open — likely a
  DC-ZVA/`DCZID_EL0` assumption); replaced with plain byte-loop
  implementations (correct, slower), linked via `--allow-multiple-definition`.
- **Security hardening TODOs** (`archive/RUMP_SYSPROXY.md` "Security /
  hardening"): the sysproxy channel fd isn't yet proven private to
  `rump_server` (a box process speaking the wire directly could impersonate
  the kernel proxy); `rump_server` isn't yet proven un-killable from inside
  its own box; per-box isolation of the proxy (box A reaching box B's stack)
  is believed correct via the namespace boundary but has no explicit
  self-test; `rumpuser__hyp` (a hyp-upcall function-pointer table) lives in
  writable `.data` and should be `mprotect`'d read-only after init.
  Acceptable for the current non-prod showcase.

## Alternatives considered and rejected

- **Userspace `LD_PRELOAD` hijack talking directly to a shared `rump_server`**
  (bypass the kernel, keep stack sharing). Rejected: it relocates the
  sysproxy *client* from kernel to userspace without removing the
  cross-process hop to `rump_server`, and adds client-side channel-I/O traps
  the kernel route folds into the original syscall trap — a wash at best.
  `archive/HIJACK_VS_KERNEL_PROXY.md`.
- **In-process rump (no server, no sysproxy)** — the original M1 approach,
  still what `rumphttp`/`hijack.c` demonstrate. Genuinely removes the
  cross-process hop, but gives up stack *sharing* (each process gets its own
  DHCP lease + ~19 kthreads), hits the one-rump-per-tap limit, and can't
  intercept musl-stdio binaries (`writev`/`readv` bypass the PLT). Kept in
  mind for a dedicated single-payload box, not a general replacement.
- **frankenlibc** (a rump-backed libc, one layer below `LD_PRELOAD`) —
  would solve the musl-stdio gap and the mixed-fd `select()` problem
  properly, aarch64 primitives look correct, but adoption is a
  multi-session, parallel build effort. Parked. `archive/FRANKENLIBC_EVAL.md`.
- **Per-process NetBSD ABI personality** (swappable per-process syscall
  table selected by the ELF loader's ABI note, à la `struct emul`/
  `sysentvec`) — the real end state for running **unmodified NetBSD
  binaries**, including prebuilt pkgsrc packages, with zero translation.
  Deferred, post-M1; no current implementation. `archive/ARCHITECTURE_QUESTIONS.md`,
  `archive/IMPLEMENTATION_PLAN.md` §10.5.

## Build

```sh
cd userspace/rumpkernel
./build.sh checkout                     # pinned src-netbsd checkout (~375 MB, once)
./docker-build.sh                       # librump*.a for aarch64-linux-musl (Alpine arm64 container)
(cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl)
./docker-build-rump-server.sh           # links rump_server (fiber by default)
```

`librump.a` + friends (304 archives, incl. the NetBSD TCP/IP stack) are built
in a Linux container, not on macOS — the pinned NetBSD source (7.99.34, 2016)
needs a laxer/older-style compiler than modern Apple clang provides; an
Alpine arm64 container is a native `aarch64-linux-musl` build with no cross
toolchain. `archive/PHASE01_BUILDRUMP.md` has the exact compiler-era shims.

### `rump_server` flags

| Flag | Effect |
|---|---|
| `--net` | Bring up networking (DHCP on `/dev/net/tap0`). |
| `--fd 3` | Use fd 3 as the sysproxy channel (kernel wire). |
| `--log <path>` | Write the server log to `<path>` (devbox: `/var/log/box/0/rump_server.log`). |

### Cargo features (`userspace/rumpkernel/rumpuser/Cargo.toml`)

| Feature | Default | Effect |
|---|---|---|
| `threads_fiber` | **on** | Cooperative fiber backend (see above). `--no-default-features` for legacy pthread. |
| `rump_server_main` | off | Compiles the Rust `rump_server` wrapper `main` into the staticlib (avoids a duplicate-`main` collision with other consumers of the shared `.a`; the rump_server build rebuilds the `.a` with this feature right before its link). |
| `rump_quiet` | off | Silences rump's own boot output. |
| `rumpuser_debug` | off | Traces every hypercall (+ memory size/align/ptr) to stderr — bring-up debugging only. |

Kernel side: `rump` feature (in `default`, so a normal `cargo build
--release` carries it; the size/extreme profiles build
`--no-default-features` and omit it). `rump-default` (`= ["rump"]`) flips box
0 itself to rump — part of the `devbox` meta-feature. See
[`config-flags.md`](config-flags.md).

## Background

- `archive/RUMP_SYSPROXY.md` — the committed shared-stack design + the full
  build-out log (sysproxy step-by-step, curl/DNS/IRC capstones, security
  TODOs).
- `archive/HIJACK_VS_KERNEL_PROXY.md` — why kernel-side routing over a
  userspace hijack; the fiber-backend cost/benefit analysis in full.
- `archive/FIBER_HANDOFF.md` — the fiber backend's operational history:
  what's done, the latency root-causing (including the disproven heartbeat
  theory), the tap-fd poll fix, the C→Rust `rump_server` port.
- `archive/RUMP_LATENCY_SLEEP_FIX.md` — the heartbeat/herd hypothesis, built,
  measured, and falsified. Kept so it isn't re-attempted.
- `archive/ARCHITECTURE_QUESTIONS.md` — the box-routing options (A-D) and
  why kernel-side routing was chosen over hijack/frankenlibc/ABI-personality.
- `archive/FRANKENLIBC_EVAL.md` — the parked frankenlibc evaluation.
- `archive/IMPLEMENTATION_PLAN.md` — the original phased build-out (Phases
  0-7, all done) and the forward-looking §10 architecture notes.
- `archive/PHASE01_BUILDRUMP.md`, `PHASE2_RUMPUSER.md`, `PHASE3_KERNEL_TAP.md`
  — the three bring-up phases (cross-build, Rust `rumpuser`, kernel tap
  device) in narrative form.
- `archive/DEV_ZERO.md` — the one non-feature-gated kernel prerequisite
  (`/dev/zero`) the rump port needed.
- `archive/NATIVE_STACK_INTERNET.md` — confirming the *native* smoltcp stack
  already had internet access, independent of this port.
- `archive/RUMP_PLUS_HERD.md` — the original herd-integration design (now
  implemented; see "The alternative: `stack=rump` herd box" above).
- `archive/OPTIONAL_SMOLTCP.md` — making rump the *only* stack for the
  devbox (compiling smoltcp out entirely).
- `archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md` — the removed per-core rump
  variant (one secondary core running its own NetBSD stack on a dedicated
  NIC) and its latency investigation; not part of the current design.
