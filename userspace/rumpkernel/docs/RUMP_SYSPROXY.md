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
   - ⏳ **Remaining integration** (needs a booted box, iterative; no architectural
     unknowns left):
     1. **In-kernel `Transport`** = read/write the kernel-held end of the pipe pair
        (reuse `src/syscall/pipe.rs`); hand the other end to `rump_server` at spawn as a
        private inherited fd (see channel-fd-isolation TODO). `rump_server` calls
        `rumpuser_sp_init_fd(fd)` instead of `rump_init_server(url)`.
     2. **In-kernel `ClientMem`** over the calling box process's user VA (with the
        mandatory server-input bounds checks — see Security TODOs).
     3. **Syscall interception** for `stack=rump` boxes: route socket-family syscalls
        to the rumpsp client instead of smoltcp.
     4. **Marshaling / translation** (the only place ABI knowledge lives): Linux/Akuma
        sysnum → NetBSD rump sysnum; arg packing into the `register_t` block; a per-box
        **fd map** (box fd ↔ rump-server fd); `struct sockaddr_in` `sin_len` fixup served
        via `ClientMem`; NetBSD errno → Akuma errno on return. (This is hijack.c's
        Linux↔NetBSD work relocated into the kernel.)
     5. **Kernel boot self-tests** per project policy.
5. **herd**: the box bundle starts the rump_server payload + sets the box's
   `stack=rump` (see `RUMP_PLUS_HERD.md`); smoltcp off = the box's only stack.
   Validate end-to-end: `/bin/curl https://ifconfig.me` in a `stack=rump` box returns
   a real answer over the NetBSD stack.

## Security / hardening TODOs
- **Seal `rumpuser__hyp` after init.** Our Rust rumpuser exports `rumpuser__hyp` (the
  rump-kernel upcall function-pointer table) as a `static mut` in writable `.data` — a
  classic control-flow-hijack target if an attacker gains an arbitrary-write primitive in
  the rump_server process (same posture as stock NetBSD `librumpuser`, not a new surface).
  It is write-once (populated in `rumpuser_init`, read-only thereafter), so a hardening
  fix is to `mprotect()` its page read-only right after init (write-once-then-seal).
  Acceptable as-is for the non-prod showcase; TODO before any non-showcase use.
- **Validate the sysproxy wire in the kernel-as-client (Step 4).** This is the *real* new
  trust boundary: the kernel will forward a box's syscall args (lengths, embedded
  pointers, copyin/copyout sizes) over the unix socket to `rump_server` via the sp_*
  protocol. All of that wire input must be bounds/sanity-checked in the kernel before use
  — never trust client-supplied lengths/offsets — or it becomes a memory-safety hole.
  Track this as a first-class requirement of the Step 4 implementation + its self-tests.
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

## Open items / risks
- musl portability of `rumpuser_sp.c`/`sp_common.c` (atomics, `INFTIM`, BSD cdefs).
- the per-fd handle map + blocking semantics in the kernel-as-client step.
- `/dev/net/tap0` reset-on-close (HANDOFF workaround #5) so a box restarts cleanly.
- fiber-vs-pthread is moot here: the server keeps our pthread rumpuser; clients don't
  carry rump at all (kernel forwards), so no per-binary concurrency model to choose.
