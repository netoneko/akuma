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
- **Channel works with smoltcp off:** `rumpuser_sp.c` listens on a *host* socket
  (`socket/bind/listen/accept/poll` on a `tcp://`|`unix://` URL). Akuma **has AF_UNIX**
  (`src/syscall/net.rs`), so use **`unix://`** — local IPC, no smoltcp needed.

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
   - ⏳ **Remaining integration** (needs a booted box, iterative):
     1. **In-kernel `Transport`** over a real AF_UNIX client connection to the box's
        rump_server socket.
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
