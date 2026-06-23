# Per-box rump server (sysproxy) — the committed shared-stack architecture

**Decision (2026-06-22):** a `--net` box gets **one rump server process** that owns the
NetBSD TCP/IP stack + `/dev/net/tap0`, and other in-box processes share it via rump's
**sysproxy** (remote-syscall) mechanism. Chosen over per-binary in-process rump
(no sharing) and frankenlibc (big adoption). End goal: **Akuma's kernel is the
sysproxy client**, so in-box programs (sshd, busybox, tcc-built sic) are *unmodified*
Akuma binaries whose `AF_INET` syscalls the kernel forwards to the box's rump server —
i.e. "kernel routing per box," built on rump's upstream RPC instead of a bespoke one.

Why this model: it's the only one where **sshd *and* a separately-compiled sic share
one stack** (different processes, same NetBSD stack) — the full acceptance/11 story.
The in-process backend gives only one networked payload per box.

## Why it's tractable (gating deps verified, 2026-06-22)
- **sysproxy server impl is in-tree:** `src-netbsd/lib/librumpuser/rumpuser_sp.c`
  (30 KB) + `sp_common.c` (16 KB) define exactly the 8 `rumpuser_sp_*` we currently
  **stub** in `rumpuser/src/lib.rs`. The client (`librumpclient/rumpclient.c`) is
  in-tree too. We skipped them (built `-k`, stubbed `sp_*`) — this is "build what we
  skipped," not "invent."
- **`rump_init_server` exists** in our built libs (`nm` ✓) — the kernel entry that
  starts the listener and calls `rumpuser_sp_init`.
- **Channel works with smoltcp off:** `rumpuser_sp.c` normally listens on a *host* socket
  (`socket/bind/listen/accept` on a `tcp://`|`unix://` URL). ~~Akuma has AF_UNIX~~ —
  **CORRECTION (2026-06-23):** Akuma's AF_UNIX is `socketpair`-only; there is **no
  path-based AF_UNIX** (`bind`/`listen`/`connect` by pathname). So the transport is a
  kernel **pipe pair** instead: the kernel hands `rump_server` one end as an inherited fd
  and serves via `rumpuser_sp_init_fd()` (no listener). Local IPC, no smoltcp. See Step 4
  "Transport shape PROVEN" below.

## Build sequence
1. **Spike — un-stub `sp_*`: ✅ DONE (2026-06-22).** `docker-sysproxy-spike.sh` compiles
   `rumpuser_sp.c` (+ its `#include "sp_common.c"`) and `rumpuser_errtrans.c` against our
   musl header env, links them with `librump.a` + our Rust rumpuser, and **`rump_init()`
   still boots** (regression clean). The 8 Rust `sp_*` stubs were removed.
   What it took (musl vs 2016 NetBSD): `apk add bsd-compat-headers` (musl lacks
   `sys/cdefs.h`/`sys/queue.h`); `-DLIBRUMPUSER -D_KERNTYPES` (opens the
   `rump/rumpuser.h` kernel-consumer guard); a musl-tuned `rumpuser_config.h`
   (`-DRUMPUSER_CONFIG`) flipping BSD-only `HAVE_*` off (no `sin_len`, no `getenv_r`, …);
   and — the real coupling — our Rust rumpuser now **exports `rumpuser__hyp`** (the
   hyp-upcall global the sp server reads by value, populated in `rumpuser_init`), while
   `rumpuser__errtrans` comes from NetBSD's standalone `rumpuser_errtrans.c`.
   `sys/atomic.h` turned out not to be needed by the sp source. **Foundation drops in.**
2. **rump_server payload: ✅ DONE (2026-06-22).** `rumpuser/rump_server.c` +
   `docker-build-rump-server.sh` build a 14 MB static aarch64-musl binary (rump +
   inet/virtif + **`-lrumpkern_sysproxy`** for `rump_init_server` + our sp objects +
   Rust rumpuser). Verified in-container: `rump_init()` boots, `rump_init_server(
   unix:///…) -> 0`, and the listening socket appears. On Akuma it additionally
   DHCPs over `/dev/net/tap0` (NIC1); in-container ifcreate/DHCP warn-fail (no tap0)
   and the listener still serves (made non-fatal on purpose).
3. **Prove sharing (client = rumpclient): ✅ DONE (2026-06-22).** `rumpuser/sp_client_test.c`
   + `docker-sysproxy-client-test.sh` build NetBSD `librumpclient` (`rumpclient.c` +
   `rump_syscalls.c`, `-DRUMP_CLIENT -D_KERNTYPES -DRUMPUSER_CONFIG`; `srcsys` symlinked
   to `src/sys/sys`) and run a second process that connects over the unix socket:
   `rumpclient_init OK` → `rump_sys_socket(AF_INET,SOCK_STREAM) -> 3` against the
   **server's** kernel → PASS. Two processes share one NetBSD stack; the sp wire
   round-trips. This is the known-good reference client for the Step-4 kernel client.
4. **Kernel as client (the payoff): 🚧 IN PROGRESS.** Akuma, for a `stack=rump` box,
   speaks the rumpsp wire to the box's server, forwarding the box processes' socket
   syscalls.
   - ✅ **Protocol core DONE + host-tested** — `crates/akuma-rump/src/sysproxy.rs`:
     the rumpsp client (`connect`/handshake + `syscall` with the COPYIN/COPYOUT/ANONMMAP
     callback loop), parameterized over a `Transport` (byte I/O) and `ClientMem`
     (the box process's user memory). 8 host tests cover header layout, guest handshake,
     syscall-with-copyin, copyout (no-response), anonmmap, ERROR→errno, errno
     propagation, and an oversize-frame guard. ABI-agnostic by design.
   - ✅ **Translation layer DONE + host-tested** — `crates/akuma-rump/src/syscall_translation.rs`
     (hijack.c ported to Rust): Linux aarch64 sysno→`Op`→NetBSD sysno map
     (socket=`__socket30` 394, etc.); `pack_args` (register_t widening, matches
     `rump_syscalls.c`); `sockaddr_in` Linux↔NetBSD (`sin_len` insert); `SOCK_NONBLOCK`/
     `SOCK_CLOEXEC` strip; NetBSD→Linux errno map (EAGAIN 35→11, EINPROGRESS 36→115,
     ECONNREFUSED 61→111, …); per-box `FdMap` (box fd ⇄ rump fd). 10 host tests. This
     is the only place ABI knowledge lives.
   - ✅ **Transport shape PROVEN — kernel pipes** (`docker-sp-fd-test.sh`, 2026-06-23).
     Akuma has no path-based AF_UNIX (only socketpair), so the transport is a kernel
     **pipe pair**: the kernel hands `rump_server` one end as an inherited fd and keeps
     the other. New: `rumpuser/sp_serve_fd.c` adds `rumpuser_sp_init_fd(connfd,...)` —
     serves the sysproxy protocol on a PRE-CONNECTED fd (no socket/bind/listen/accept),
     reduced from NetBSD's `spserver`/`serv_handleconn` (it `#include`s the unmodified
     `rumpuser_sp.c` to reach its statics). `rumpuser/sp_fd_test.c` proved it in one
     process: socketpair → `rumpuser_sp_init_fd` on one end → raw sp client on the other
     → `rump_sys_socket -> fd 3` through the rump kernel. PASS. The raw client mirrors
     `sysproxy.rs`, cross-checking its framing.
   - ✅ **Kernel-as-client PROVEN ON AKUMA (2026-06-23).** `src/rump_proxy.rs`
     (`#[cfg(feature="rump")]`, boot demo gated on `rump_tap::is_ready()`): the kernel
     creates a pipe pair, spawns `/bin/rump_server --fd 3` and installs the server end at
     fd 3 (`set_fd(UnixSocket{rx,tx})`), then runs `akuma_rump::sysproxy::Client` over a
     `PipeTransport`/`KernelPipeIo` and drives `rump_sys_socket → fd 3` through the rump
     kernel. Live boot log: banner → handshake → `rump_sys_socket -> fd 3`, then
     `kill_process` reaps the server + its ~19 kthreads (no leak; `ps` shows only
     herd/httpd after). rump_server stdout → `/var/log/box/<id>/rump_server.log` (`--log`)
     since the kernel can't drain its ProcessChannel that early in boot. Bug fixed en
     route: the HANDSHAKE reply is a short non-`rsp_sysresp` word, so `connect()` must
     accept a RESP without `parse_sysresp` (regression-tested).
   - **ARCHITECTURE: the proxy is PER-BOX, in a per-box kthread.** Each `stack=rump`
     box gets its **own** kernel-side proxy — one proxy **kthread per box**, owning that
     box's pipe channel + fd map + rump_server connection, **blocking** on its channel
     (not busy-yielding). A single proxy serving all boxes is wrong. The boot
     `rump_proxy::run_demo()` (single-shot, box 0, on the boot thread, cooperative-yield)
     was only a transport stepping-stone — NOT the architecture; it gets replaced by the
     per-box kthread.
   - **Idle-loop fix (2026-06-23):** `rump_server`'s idle `for(;;) pause()` busy-looped —
     Akuma has no `pause` syscall, so musl `pause()` → `ppoll(NULL,0)` and `sys_ppoll`
     returns immediately for `nfds==0` → CPU peg (the `ppoll=16.8M` storm, in *both*
     `--net` and not). Fixed: idle via `sleep()` (→ `nanosleep`, which blocks). (Kernel
     latent quirk to revisit: `ppoll(nfds=0)` should block until a signal, not return 0.)
   - ✅ **Phase A — dispatch instrumentation + `stack=rump` wiring (2026-06-23).**
     `SET_BOX_STACK` syscall (324); herd reads `stack = rump` from a `.conf`
     (`rumpnet.conf`) and calls it after `register_box`; the kernel keeps a
     `RUMP_BOXES` set (`rump_proxy::{mark_box_rump,box_is_rump}`). A non-breaking
     trace (`[RUMP-SP]`) in `handle_syscall` logs a rump box's socket-family
     syscalls, then falls through to smoltcp. **It revealed curl's exact
     sequence** (committed): curl does **its own DNS over a UDP socket**
     (`socket(AF_INET,DGRAM)`/`bind`/`sendto`/`recvmsg`/`close`) *then* the TCP
     connection (`socket(AF_INET,STREAM,proto=6)`/`setsockopt`×5/`connect`/
     `getsockname`/`getsockopt(SO_ERROR)`/`sendto`/`recvfrom`×N/`close`). Findings
     that reshaped the plan:
       - curl's **first** call is `socket(AF_INET6=10,…)` — the proxy must return
         `EAFNOSUPPORT` (not proxy it) so curl falls back to IPv4.
       - **UDP + `bind` + `recvmsg`** are on the DNS hot path (a TCP-only proxy
         never resolves a name); `recvmsg` needs `msghdr`/iovec marshaling.
       - curl uses **`sendto`/`recvfrom`, never `read`/`write`** on the socket.
       - box fds are low (4,5) → register a real `FileDescriptor::RumpSocket`
         (normal low fd, `poll`/`select`-compatible) and keep the box-fd→rump-fd
         map *inside* that fd, NOT the `0x40000000+` `FdMap` (that breaks `fd_set`).
   - 🚧 **Phase B (approach 1 — synchronous on the calling thread).** Decided
     (vs the doc's per-box kthread): the box's *own* syscall thread drives the
     rumpsp round-trip under a per-box cooperative lock, so `copyin`/`copyout`
     trivially hit the **calling process's** VA (`current` TTBR0) — matching the
     proven `run_demo`. The kthread/request-queue model is deferred until this is
     proven (its `copyin` would need a cross-address-space page-table walk).
     - ✅ **B1 — per-box lazy bring-up + `socket`/`close` round-trip (2026-06-23).**
       `rump_proxy.rs`: `PROXIES` map (`Initializing`/`Ready`/`Failed`), `BoxProxy`
       (the handshaken `Client` behind a take/replace slot — the guarding spinlock
       is never held across the yielding channel read), `ensure_box_proxy` +
       `setup_proxy` (spawns `/bin/rump_server --fd 3 --log …` into the box on
       first `socket()`, installs the channel fd, handshakes). `intercept_box_syscall`
       in `handle_syscall` returns `Some(result)` to short-circuit smoltcp.
       **Validated on Akuma:** curl in the `rumpnet` box → `socket(AF_INET6)`
       `EAFNOSUPPORT`, `socket(AF_INET)` spawns the server (`SERVING sysproxy on
       fd 3`) and **round-trips to a real `rump_sys_socket`** → box fd 5, `close`
       round-trips. No crash, VM healthy.
       - **Bug fixed (self-interception):** the sysproxy server drives its channel
         fd with socket `sendto`/`recvfrom` (NetBSD `rumpuser_sp.c` is written for
         sockets), and it runs *inside* the `stack=rump` box, so its own channel
         I/O was being intercepted → handshake deadlock. Fix: record the server
         pid in `SERVER_PIDS` the instant it spawns (before the handshake) and
         exclude those pids from interception — they fall through to the normal
         dispatch that handles the pipe-backed `UnixSocket` fd (as box-0 `run_demo`
         does).
       - **`net=off` here is deliberate:** B1 ran the server WITHOUT `--net`, so
         the NetBSD **data plane is down** (no `virt0`/DHCP/route). `socket`/`close`
         are pure rump-kernel object ops and work; an actual `connect` to an IP
         needs `--net` (B3). The fd-3 channel (control plane) is independent of
         `net` and always present.
     - ⏳ **B2 — TCP-path marshaling + real `ClientMem`** (in progress): `ProcMem`
       (copyin/copyout over the calling proc's VA + `sockaddr_in` Linux↔NetBSD
       translation on the pointer args, size-capped by `MAX_TRANSFER`); marshal
       `connect`/`getsockname`/`sendto`/`recvfrom`/`read`/`write`; `getsockopt(SO_ERROR)`
       short-circuits to 0 (the rump socket is kept blocking, so `connect` completes
       synchronously — no pending error); `setsockopt` best-effort no-op (curl
       tolerates). Then `connect` round-trips (→ `ENETUNREACH` until `--net`).
     - ⏳ **B3 — `--net`**: bring up the rump stack (`virt0` + DHCP over
       `/dev/net/tap0`) so `connect`/`sendto`/`recvfrom` reach the real wire.
       **Risk:** the known `--net` DHCP `ppoll` busy-loop (see "Open items"); may
       need DHCP to block rather than spin. Validation target:
       `curl -H Host:ifconfig.me -L http://34.160.111.145` over the NetBSD stack.
     - ⏳ **Open sub-problem:** `poll`/`ppoll` on a `RumpSocket` fd (curl's nonblock
       loop) isn't intercepted yet — may need `RumpSocket` to report ready so curl
       proceeds to the (blocking) recv.
     - ⏳ **Kernel boot self-tests** per project policy.

   ### ✅ B-OUTCOME (2026-06-23): curl over rump WORKS; final ownership = herd
   - **`curl -H Host:ifconfig.me http://34.160.111.145` over the NetBSD rump stack
     returns `87.71.13.205`, repeatably.** The full TCP/HTTP path is marshaled:
     `socket`/`close`/`connect`/`getsockname`/`getpeername`/`getsockopt(SO_ERROR→0)`/
     `setsockopt(no-op)`/`sendto`/`recvfrom`/`read`/`write` via `ProcMem` (user-VA
     copyin/copyout + sockaddr Linux↔NetBSD translation) + `FileDescriptor::RumpSocket`.
   - **Ownership decided (user): herd owns the `rump_server` PROCESS; the kernel
     owns the CHANNEL + proxy.** `rumpnet.conf`: `command=/bin/rump_server`,
     `args=--net --fd 3`, `stack=rump`, `restart=false`. herd calls `SET_BOX_STACK`
     BEFORE spawning; when the spawn lands, `sys_spawn_ext` detects it
     (`box_is_rump` + path "rump_server") and calls `rump_proxy::attach_server`,
     which installs the kernel pipe pair on the server's fd 3 (before it runs) and
     **handshakes in a kthread** (blocks ~5s through rump_init + DHCP), publishing
     the `BoxProxy` to `PROXIES`. The earlier lazy/kernel-eager-spawn approaches were
     dropped. `restart=false` is deliberate: a correct restart must re-establish the
     channel/proxy + back off — that health/lifecycle work is **TBD**. Detection by
     path-match is interim; a herd→kernel notify is the cleaner TBD trigger.
   - **`herd` gained a `restart` config flag** (default true); `check_process_exits`
     honors it. `ServiceConfig.restart`.
   - ⚠️ **LATENCY — root-caused, not yet fixed (the remaining work).** Each forwarded
     syscall costs ~0.8–4s (a 74-byte `sendto` = ~0.8s with NO network wait → pure
     channel/scheduling cost); full curl ~26s. **Cause:** the rump_server's ~19 rump
     kthreads **busy-spin in userspace** — `ps` shows them `STATE running, SYSCALL -`
     (NOT blocked in a syscall; only the main thread sits in futex `*98`). They
     contend for the rump kernel's single virtual CPU as a **spin**, so a thread that
     needs the CPU (e.g. to read the channel / run the proxied syscall) waits behind
     ~19 spinners → ~270ms per scheduling hop, ~4 hops/syscall. Evidence it's
     thread-count-bound: removing a (then-duplicate) second rump_server **halved**
     per-op latency (linear). Two fixes that did **NOT** help (ruling out poll
     cadence / client busy-spin): making `sys_ppoll` event-driven like epoll (waker
     registers on the channel pipe), and making the kernel proxy's channel read sleep
     instead of busy-yield (`KernelPipeIo::yield_now`). **THE fix (B, next):** make
     idle rump kthreads / the rump-CPU wait TRULY BLOCK (futex sleep) instead of
     spin — in our Rust `rumpuser` (`rumpuser/src/lib.rs`: the `cv_wait`/scheduler
     CPU-wait / spin-mutex paths). Needs the container rebuild + relink of
     `rump_server`. (Fiber rumpuser — one OS thread per kernel — is the deeper
     alternative; see "Future optimizations".)
   - ✅ **`bootstrap/bin/sic`** — static sic 1.3 (aarch64-musl, 130 KB) for the IRC
     capstone. Connect by IP (`sic -h <ip> -p 6667 -n <nick>`; no DNS). Blocked on:
     `poll`/`select` readiness for `RumpSocket` fds (sic `select()`s the socket) +
     usable latency.
5. **herd**: the box bundle starts the rump_server payload + sets the box's
   `stack=rump` (see `RUMP_PLUS_HERD.md`); smoltcp off = the box's only stack.
   Validate end-to-end: `/bin/curl https://ifconfig.me` in a `stack=rump` box returns
   a real answer over the NetBSD stack.

## Box demo status (2026-06-23)
- ✅ **herd autostarts a boxed NetBSD rump kernel.** `RUMP_NIC=1` boot → herd starts the
  `rumpnet` service **boxed**; `ps` shows `/bin/rump_server` + its ~18 rump kthreads under
  a non-zero box (hex `185c61f8b7` / decimal `104629139639`, named `rumpnet`). Idle and
  stable; VM stays responsive. (Required three fixes: herd `spawn_in_box` ABI [argv
  pointer-array + options at arg2]; kernel `sys_spawn_ext` always stripping `argv[0]`
  [was leaking the path as a positional arg → `rump_init_server("/bin/rump_server")`];
  and `rump_server` idling via `sleep` not `pause` [Akuma `ppoll(nfds=0)` returns
  immediately, so `for(;;) pause()` busy-pegged the CPU].)
- ✅ **A process runs inside the box.** `box use rumpnet -i /bin/busybox sh` →
  `/bin/busybox sh` runs under box `185c61f8b7` (confirmed in `ps`). Box plumbing works.
- ⚠️ **box-id resolution gotcha.** `ps` prints the box id in **hex** (`185c61f8b7`) but
  `box use` / `/proc/boxes` use **decimal** (`104629139639`); `resolve_target_id` only
  accepts bare hex with a `0x` prefix, so `box use 185c61f8b7` falls through to a name
  lookup and misses. Working forms: `box use rumpnet`, `box use 104629139639`,
  `box use 0x185c61f8b7`. TODO: make the resolver accept the hex form `ps` prints.
- ✅ **curl (musl) runs + does real HTTPS from inside the box** — via `box use`:
  `box use rumpnet -i /bin/curl -sS https://ifconfig.me/ip` → `87.71.13.205` (mbedTLS).
  **This is over SMOLTCP ONLY — NOT the NetBSD/rump stack.** The box shares the kernel's
  native AF_INET, so curl-in-a-box is just curl-over-smoltcp-in-a-box; it does **not**
  validate the rump path. Launched via `box use` (kernel SPAWN_EXT), which loads musl
  binaries fine. (`/bin/curl` is the real static curl+mbedTLS; `/bin/curl-static` does
  NOT exist — running that path is what "segfaulted"/"box use: failed", a missing binary,
  not a real bug.)
- ✅ **In-box fork+exec works for native binaries — including networking.** `busybox sh`
  inside the box → `/bin/hello` runs; → `/bin/meow` (~250 KB, native libakuma) runs AND
  talks to ollama over smoltcp. So fork+exec + in-box userspace networking work — over
  **smoltcp**, not rump.
- ⚠️ **OPEN — in-box fork+exec of a large *dynamic* binary faults.** `busybox sh` →
  `busybox wget` (busybox re-exec) SIGSEGVs (`[DP] no lazy region for inst FAR=0x0` →
  `WILD-IA ELR=0x0`, entry=0). `/bin/curl` runs fine via `box use` (SPAWN_EXT), so this
  is specific to the *in-box fork+exec* path (not SPAWN_EXT, not box 0). Lower priority
  than the proxy. (Earlier framing as a "musl exec bug via curl-static" was wrong —
  curl-static doesn't exist; that path was a missing-binary failure.)
- 🚧 **IN PROGRESS — curl over the NetBSD/rump stack** (Step-5 target). **B1 done
  (2026-06-23):** a box binary's `socket()`/`close()` now route through the per-box
  proxy to a real `rump_sys_socket` (validated with curl in the `rumpnet` box; see
  Step 4 "Phase B"). The connection path (`connect`/`sendto`/`recvfrom`/`getsockname`)
  is the in-progress B2 marshaling, and `--net` (B3) is needed before any of it
  reaches the wire. So: dispatch + socket round-trip proven; actual end-to-end HTTP
  (`curl -H Host:ifconfig.me -L http://34.160.111.145`) still pending B2+B3.

## Security / hardening TODOs
- **Seal `rumpuser__hyp` after init.** Our Rust rumpuser exports `rumpuser__hyp` (the
  rump-kernel upcall function-pointer table) as a `static mut` in writable `.data` — a
  classic control-flow-hijack target if an attacker gains an arbitrary-write primitive in
  the rump_server process (same posture as stock NetBSD `librumpuser`, not a new surface).
  It is write-once (populated in `rumpuser_init`, read-only thereafter), so a hardening
  fix is to `mprotect()` its page read-only right after init (write-once-then-seal).
  Acceptable as-is for the non-prod showcase; TODO before any non-showcase use.
- **sysproxy wire bounds-checks — TBD (refined plan, 2026-06-23).** The trust boundary is
  the sp wire (server-supplied copyin/copyout lengths + addresses). Refined strategy:
  - **Kernel-as-client `ClientMem`**: only **sanity-check the size** (a sane cap +
    that the copy stays within the box process's mapped VA via `copy_from/to_user_safe`,
    which already faults on bad addresses). `MAX_TRANSFER` in `sysproxy.rs` is the coarse cap.
    The kernel does *not* attempt full validation of every field.
  - **The server checks the rest**: comprehensive request validation (well-formedness,
    field consistency, the actual `rsp_*` invariants) belongs in the sp **server**, and
    lands when we **port the sp glue (`rumpuser_sp.c`/`sp_serve_fd.c`) → Rust** (HANDOFF
    workaround #4) — that Rust port is where the extra checks live, not the C as-is.
  So: kernel = "is the size sane + does it fit the process VA"; server (post-Rust-port) =
  "is the request valid". Track both; neither blocks the showcase.
- **The sysproxy channel fd is private to rump_server (TODO).** With the kernel-pipe
  transport, the server-end fd handed to `rump_server` must be reachable by **only**
  that process: not inheritable by other box processes (no leak across spawn), not
  dup-able/openable from another box fd, and the kernel-held end never exposed to
  userspace at all. Otherwise a box process could speak the sysproxy wire directly and
  inject syscalls into the rump kernel, impersonating the kernel/proxy. Enforce +
  self-test: another box process cannot read/write or even reference the channel fd.
- **rump_server is not killable from inside the box (TODO).** The `rump_server` is the
  box's stack daemon; if an in-box process could signal/kill it, that process could DoS
  the whole box's networking (and orphan the kernel's per-box fd map / client connection).
  Its lifecycle must be owned from **outside** — herd/kernel start and stop it; in-box
  processes must not be able to `kill()` it, `close` its socket out from under the proxy,
  or otherwise reach it as a normal box PID. Enforce + self-test: an in-box `kill` of the
  server PID is denied, and only herd/`box close`/kernel teardown stops it. (Relates to
  per-box isolation below.)
- **Verify per-box isolation of the proxy (TODO for later).** Each box's `rump_server`
  listens on a unix socket in *that box's* mount namespace, and the kernel-as-client must
  forward a box's AF_INET syscalls *only* to that same box's server. Box A must not be
  able to reach, connect to, or even enumerate box B's server socket / stack / interfaces.
  The namespace boundary should provide this, but add explicit self-tests: a process in
  box A cannot open box B's sysproxy socket, cannot see box B's `virt0`, and the kernel
  refuses to route box A's calls to box B's server. (Containment must be proven, not assumed.)

## Future optimizations
- **Fiber rumpuser → one OS thread per NetBSD kernel (TODO).** Our `rumpuser` uses the
  **pthread** threading backend, so each rump kthread (`rump_init` spawns ~15–20:
  per-CPU workqueues, softints, pagedaemon, cprng/aiodone, …) is a real Akuma thread —
  visible as ~19 child PIDs per `rump_server`. Functionally fine (and pleasingly
  cyberpunk), but at cluster scale (N boxes × ~19 threads) it adds up. Rump's **fiber**
  backend (`rumpfiber.c`, `RUMPUSER_THREADS=fiber`, ucontext green threads) collapses
  these to ~one OS thread per kernel. Deferred: it's a different concurrency model, and
  our `cv_wait`/`mutex`/`clock_sleep` scheduler-wrap fixes were built around pthread
  (see `FRANKENLIBC_EVAL.md` / the parked fiber notes).

## Open items / risks
- **`--net` DHCP busy-loops on `ppoll` (TBD).** When `rump_server --net` runs boxed at
  boot, PID shows ~16.8M `ppoll` (706K/s) — DHCP is spinning, not blocking, pegging the
  CPU (it reset SSH). Not a box-access issue (the tap open at `fs.rs:1100` is a literal
  path match, no `box_id` gate; `rump_tap` is a fresh global the net=off demo never
  touched). Fix direction: **DHCP must not busy-spin** — it should **back off and retry,
  bounded to ~10s total**, then give up cleanly (so a missing lease doesn't peg the CPU).
  Deferred until rumpnet boots properly without `--net`; revisit with the tap-RX /
  one-rump-per-boot angle (workaround #5).
- musl portability of `rumpuser_sp.c`/`sp_common.c` (atomics, `INFTIM`, BSD cdefs).
- the per-fd handle map + blocking semantics in the kernel-as-client step.
- `/dev/net/tap0` reset-on-close (HANDOFF workaround #5) so a box restarts cleanly.
- fiber-vs-pthread is moot here: the server keeps our pthread rumpuser; clients don't
  carry rump at all (kernel forwards), so no per-binary concurrency model to choose.
