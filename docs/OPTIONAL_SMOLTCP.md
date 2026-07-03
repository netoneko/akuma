# Optional `smoltcp` — the rump-only (devbox) build

## Why

The devbox (`overlays/devbox/`) makes the NetBSD **rump** TCP/IP stack the *default*
network stack for box 0 (see `rump-default` in `Cargo.toml` and
`rump_proxy::start_default_stack`). To make that a genuinely rump-only image — and to
reclaim the ~2 MB the native stack costs — the `smoltcp` native stack (and the in-kernel
SSH server, which is built on it) can now be compiled **out** entirely via a new
`smoltcp` cargo feature.

Result: the devbox kernel drops from **3.5 MB → 1.4 MB** with `smoltcp` off.

## The feature

`smoltcp` is **default-on** in both `akuma` (root) and `akuma-net`. Every existing build
keeps the native stack; only a `--no-default-features` build that omits `smoltcp` drops it.

```
# root Cargo.toml
smoltcp = { version = "0.12.0", …, optional = true }        # was non-optional
akuma-net = { path = "crates/akuma-net", default-features = false }
[features]
default  = [ …, "smoltcp", … ]
smoltcp  = ["dep:smoltcp", "akuma-net/smoltcp"]

# crates/akuma-net/Cargo.toml
smoltcp = { …, optional = true }
[features]
default = ["smoltcp"]
smoltcp = ["dep:smoltcp"]
```

`kernel-tls`/`tls-rsa` are kept **orthogonal** to `smoltcp` (the TLS/verifier crates are
smoltcp-free), but their only runtime consumer — `http_get` (shell `curl https://`) — is
smoltcp-coupled, so with `smoltcp` off they are dead weight; the devbox omits them.

### Profiles that build `--no-default-features` must now list `smoltcp` explicitly

Because `smoltcp` is optional, any profile built with `--no-default-features` that still
wants the native stack has to re-add it:

- `scripts/build_size.sh` — added `smoltcp` (keeps native stack + built-in SSH + HTTPS).
- `scripts/build_extreme_size.sh` — added `smoltcp` (unchanged behavior; drop it later to
  reclaim space if extreme goes netless).
- `scripts/build_devbox.sh` / `overlays/devbox/run.sh` — deliberately **omit** `smoltcp`
  (and `kernel-tls`/`tls-rsa`); rump is the only stack.

The `smp` builds keep the default feature set, so they still get `smoltcp`.

Devbox build line:
```
cargo build --profile devbox --no-default-features \
  --features devbox,neko,sound,no-tests,\
sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll
```

## The gating split

`smoltcp` is woven through ~15 kernel files plus `akuma-net`, and the in-kernel SSH server
is built on smoltcp sockets — so **smoltcp and the built-in SSH are compile-coupled** and
drop together. The code splits into three tiers:

- **Tier A — smoltcp-free, kept as-is.** `akuma-net` `runtime`, `hal`, `stats`,
  `rump_tap`, and the whole `akuma-rump` crate; and in `socket.rs` the *types* used
  pervasively by non-network code: `SockAddrIn`, `SocketAddrV4`, `socket_const`,
  `SocketStat`, `libc_errno`. `rump_proxy.rs` itself is smoltcp-free.
- **Tier B — smoltcp-coupled, gated `#[cfg(feature = "smoltcp")]`.** `akuma-net`
  `smoltcp_net`, `dns`, `http`, and the socket-table internals of `socket.rs`; in the
  kernel: the socket ops in `syscall/net.rs`, the `Socket` fd arms in
  `syscall/{poll,fs,term}.rs`, the whole in-kernel `ssh` module, `shell/commands/net.rs`,
  and the network/ssh boot tests.
- **Tier C — unconditional callers, satisfied by stubs.** `socket::remove_socket` and
  `socket::list_sockets` are called from FD teardown, `/proc/net/tcp`, and the
  `ExecRuntime` callback regardless of stack, so `socket.rs` provides
  `#[cfg(not(feature = "smoltcp"))]` no-op / empty-Vec stubs. `sys_socketpair`
  (pipe-backed) and `sys_shutdown` stay ungated.

The socket-syscall dispatch in `syscall/mod.rs` returns `ENETDOWN` for the smoltcp-only
ops when the feature is off — **except** `SENDTO`/`RECVFROM`, which are always dispatched
(see next section).

A crate-level `#![cfg_attr(not(feature = "smoltcp"), allow(dead_code))]` in `main.rs`
covers the in-kernel shell/editor/async-fs surface that is reachable only through the
built-in SSH server: it is dead in a rump-only image (busybox over the userspace `/bin/sshd`
is used instead) but is not smoltcp-specific, so gating each item individually would be
wrong. `default`/`size`/`extreme` keep dead-code denied.

## `send`/`recv` on the fd-3 sysproxy channel must survive without smoltcp

box 0's `rump_server` is excluded from box interception (`SERVER_PIDS`), so its own
`send()`/`recv()` on the fd-3 sysproxy channel — a **UnixSocket (pipe-backed)** fd — fall
through to native dispatch. `sys_sendto`/`sys_recvfrom` therefore keep a
`#[cfg(not(feature = "smoltcp"))]` variant that handles the UnixSocket case
(`fs::sys_write`/`fs::sys_read` on the pipe) and `EBADF`s everything else, and their
dispatch arms are **not** gated to `ENETDOWN`. Without this the rump handshake banner send
fails and box 0's rump stack never comes up. (See `syscall/net.rs` +
`syscall/mod.rs` SENDTO/RECVFROM.)

## Status — WORKING

| Build | Compiles | Runtime |
|-------|----------|---------|
| default (`smoltcp` on) | ✅ clean, clippy-clean | unchanged |
| `size` / `extreme` (smoltcp re-added) | ✅ | unchanged |
| **devbox (`smoltcp` off)** | ✅ clean, clippy-clean, **1.4 MB** | ✅ **rump default stack + interactive SSH-over-rump verified** |

Verified end-to-end on the fully smoltcp-free build: box 0's `rump_server` boots (DHCP
`10.0.2.15`, `SERVING sysproxy on fd 3`), the kernel handshake completes (`box=0 proxy
ready`), herd's userspace `sshd` binds/accepts over rump, and an **interactive SSH session
runs commands** (`echo`, `uname -a`, `ls /`) with output returned — all over the NetBSD
rump stack, no smoltcp compiled in.

### Two gating bugs found + fixed (no NetBSD-source patch)

Getting there surfaced two real bugs, both from over-gating socket syscalls that the rump
path still needs. Neither is in the NetBSD source (`rumpuser_sp.c` is byte-identical to the
working smoltcp build); both fixes live in our kernel dispatch:

1. **`sendmsg` UnixSocket passthrough (rump bring-up).** box 0's `rump_server` is excluded
   from box interception, so its own channel I/O falls through to native dispatch. Its
   sysproxy replies — the handshake RESP and *every* proxied-syscall reply — go through
   `dosend` → `host_sendmsg` (only the initial banner uses `send`→`sendto`). Gating
   `sys_sendmsg` to `ENETDOWN` made the RESP fail, so the handshake timed out and the stack
   never came up. Fix: a `#[cfg(not(feature = "smoltcp"))]` `sys_sendmsg` variant that
   writes every iovec to the UnixSocket tx pipe, dispatched unconditionally (same pattern
   as the `sendto`/`recvfrom` UnixSocket variants). `readframe` uses `read`, already
   handled. (`src/syscall/net.rs`, `src/syscall/mod.rs`.)

2. **WAITPID pid ↔ rump-fd collision (session hang), and the fcntl-ownership invariant.**
   `rump_proxy::intercept_box_syscall` treated *any* syscall whose `args[0]` numerically
   matched a rump socket fd as proxy-owned. `sshd`'s `waitpid(child_pid)` (nr 303) on its
   shell child, whose pid `4` equalled the accepted rump-socket fd `4`, was thus misrouted
   and returned `EOPNOTSUPP` in a tight retry loop → the session hung. (Phase 1's larger
   pid/fd numbers never collided; the minimal `no-tests` build makes them small and
   collide.) Fix: a syscall with no translation op is owned only if it is **socket-family
   by number, OR `fcntl`/`ioctl` on a rump fd** — `args[0]` is not reliably an fd for
   arbitrary syscalls (WAITPID/KILL/SPAWN take a pid). The `fcntl`/`ioctl` carve-out is
   essential: the accept path deliberately clears O_NONBLOCK so the box sees a
   kernel-side-blocking stream, and that invariant relies on the box's own
   `fcntl(F_SETFL,O_NONBLOCK)` being proxy-owned (EOPNOTSUPP), not run natively. Letting it
   run natively flipped `box_fd.nonblock`, so the proxy started doing non-blocking rump
   recvs → EAGAIN → the SSH session dropped on the *second* connection (fd reuse, same
   flow). Read/write/close on a rump fd and socket-family ops are still owned as before.
   (`src/rump_proxy.rs`.)

### Backlog

- **One-shot `ssh host <cmd>` doesn't spawn the child** (`ssh -p 2223 root@localhost echo
  hi` closes without output); the **interactive** session works. Appears to be a
  sshd one-shot-exec path issue, not rump/smoltcp — to investigate separately.
- **Concurrent SSH sessions don't work** — `userspace/sshd` is single-session by design:
  its accept loop runs `block_on(handle_connection(...))` to completion before the next
  `accept()` (`userspace/sshd/src/main.rs:144-171`). So a second simultaneous connection
  waits for the first to finish. Not a kernel/rump bug. **Recommended fix + why (see the
  Concurrent-SSH section below).** Do it as a *single-threaded cooperative multiplexer*, NOT
  by spawning kernel threads per connection — a thread-per-connection sshd would turn sshd
  into a multithreaded process and hit the exact `curl https` deadlock class below.
- **`curl https://host` wedges the kernel — a multithreaded fault/lock deadlock (WIP).**
  See the section below.

## Concurrency: `curl https://host` wedges the kernel (WIP)

Dogfooding surfaced this and it is **not fully fixed**. Symptoms and findings, recorded so
the next session can pick it up:

**What works:** `curl --version`, `curl https://<IP>` (→ HTTP 301, so TLS-over-rump is
fine), single-threaded `curl http://host` (DNS-over-rump resolves). **What breaks:**
`curl https://host` (and, intermittently, any curl that spins up its `AsynchDNS` resolver
**thread** alongside the TLS/main thread) — i.e. the trigger is a **multithreaded** process
doing concurrent work on the rump box, not the networking itself. Concurrent SSH sessions
would hit the same class once sshd is made concurrent.

**Mechanism (two nested-fault deadlocks on the single CPU):**
1. **FIXED — `get_user_copy_fault_handler` reentrancy.** It took `POOL.lock()` from the
   data-abort handler; if a user copy faulted while `POOL` was held (nested fault), it
   self-deadlocked spinning on the pool spinlock (observed as an endless
   `qemu: … unhandled exception ec=0x20` at the `ldaxrb`/`stxrb` loop inside that fn, which
   flooded the log to 15M+ lines and spun the CPU). Fixed by moving the handler to a
   lock-free per-thread atomic array `USER_COPY_FAULT_HANDLER` (mirrors `CURRENT_TRAP_FRAME`,
   which is lock-free for the same "read from the exception handler" reason).
   `crates/akuma-exec/src/threading/{mod.rs,types.rs}`.
2. **OPEN — a second deadlock in the same class, now PINPOINTED.** With #1 fixed,
   `curl https://host` no longer produces the `ec=0x20` loop, but the kernel now **silently
   freezes** (heartbeat stops → IRQs masked forever) around the `mprotect`/demand-paging the
   DNS thread does. A full audit of the locking discipline on the mprotect + fault +
   PMM/heap + `vm_lock` + CLONE_VM paths (2026-07-03) found that **the earlier "~10 bare
   `POOL.lock()`" hypothesis was WRONG** — every `POOL.lock()` in `threading/mod.rs` is in
   fact already wrapped in `with_irqs_disabled` (or taken from IRQ context, e.g. the
   scheduler). Likewise PMM (`src/pmm.rs`), the heap (`src/allocator.rs`), `LAZY_REGION_TABLE`,
   `PROCESS_TABLE`, `Process::vm_lock`, and the page-table walkers are all uniformly
   IRQ-safe. `mprotect` itself (`src/syscall/mem.rs:602`) is clean.

   **The single discipline outlier is `Process::fault_mutex`.** It is a bare
   `spinning_top::Spinlock<BTreeMap<usize,usize>>` (`crates/akuma-exec/src/process/mod.rs:208`)
   — the per-page demand-paging "who's faulting this page" slot map — and it is the **only**
   shared spinlock on these paths acquired **without** `with_irqs_disabled`:
   - `fault_slot_acquire` → `proc.fault_mutex.lock()` at
     **`crates/akuma-exec/src/process/children.rs:251`**
   - `fault_slot_release` → `proc.fault_mutex.lock()` at
     **`crates/akuma-exec/src/process/children.rs:281`**

   Both are reached from the EL0 demand-paging fault path
   (`src/exceptions.rs:2458 / 2579 / 3098`), which runs **IRQs-ENABLED**. `spinning_top::lock()`
   does not touch DAIF, so a thread can be **preempted by the timer/SGI while holding
   `fault_mutex`**. All CLONE_VM siblings (curl's DNS thread + main thread) **share the
   leader's one `fault_mutex`** (`children.rs:511`), so they genuinely contend on it. A
   bare-vs-bare contention self-heals (the spinner gets preempted, the holder resumes), but
   the freeze becomes **permanent** the moment the *contending* acquisition happens with IRQs
   masked — i.e. a nested/EL1-side fault, or any acquisition reached from inside an
   `IrqGuard`/`with_irqs_disabled` region, spins on a preempted holder that can never be
   rescheduled (timer is masked) → heartbeat dead. This matches the reported symptom exactly.

**It's a `dispatch`/locking bug, not a rump-protocol bug** — the rump proxy dispatch itself
(fd table, `with_client`, the sendto/recvfrom/sendmsg variants) is correctly locked; the
deadlock is in the kernel's fault-handling locking under a multithreaded EL0 process.

### The fix (primary suspect — verify this first)

Make `fault_mutex` IRQ-safe like every other shared spinlock in these paths: wrap **only the
critical section** (the `.lock()` + `BTreeMap` ops) in `with_irqs_disabled`, **not** the
`yield_now()` — the loop already drops `faults` before yielding (`children.rs:269` closes the
block before `children.rs:271`), and the yield must run IRQs-enabled so the scheduler + IRQs
can make progress. `with_irqs_disabled` is reentrant, and `lookup_process`/`BTreeMap::insert`
(which may hit the IRQ-safe heap lock) nest fine. Sketch for `fault_slot_acquire`:

```rust
loop {
    let outcome = crate::runtime::with_irqs_disabled(|| {
        let proc = match lookup_process(as_owner) { Some(p) => p, None => return Some(FaultSlot::NoProc) };
        let mut faults = proc.fault_mutex.lock();
        match faults.get(&page_va).copied() {
            None => { faults.insert(page_va, my_tid); Some(FaultSlot::Acquired) }
            Some(h) if h == my_tid => Some(FaultSlot::Acquired),
            Some(h) => {
                if crate::threading::is_thread_terminated(h) { faults.insert(page_va, my_tid); return Some(FaultSlot::ReclaimedDead(h)); }
                if spins >= FAULT_SLOT_SPIN_BOUND      { faults.insert(page_va, my_tid); return Some(FaultSlot::ReclaimedWedged(h)); }
                None // contended — retry after yielding (IRQs on)
            }
        }
    });
    if let Some(slot) = outcome { return slot; }
    spins = spins.wrapping_add(1);
    crate::threading::yield_now();
}
```
and wrap the whole `if let Some(proc) …` body of `fault_slot_release` (children.rs:280-285)
in one `with_irqs_disabled`. Once IRQ-safe, a holder can never be preempted mid-critical-
section on a single CPU, so no masked contender can ever spin forever.

**Also harden (same class, off the freeze path, do while you're here):** `IRQ_HANDLERS`
(`src/irq.rs:85`) is taken bare in `register_handler` but from IRQ context in `dispatch_irq`
— same hazard, but registration is boot-time only so it's not the curl freeze.

### How to verify (for whoever picks this up)

1. Apply the `fault_mutex` fix; `cargo test -p akuma-exec --target <host>` (fault-slot
   regression tests live in `src/process_tests.rs:5330`) + `cargo build`.
2. `overlays/devbox/bootstrap.sh` then `overlays/devbox/run.sh`; SSH in and run
   `curl https://<hostname>` (the multithreaded AsynchDNS path). Success = it returns without
   freezing the VM; the heartbeat keeps ticking in the boot log.
3. If it still freezes, confirm with lldb+gdbstub (INSTANCE=1 GDB=1, attach to :1235 — see
   memory `akuma_lldb_gdbstub_debugging`): catch the freeze, `bt` all threads, and look for a
   thread spinning in `fault_slot_acquire`/`lock()` while another holds the same
   `fault_mutex`. If the stuck lock is something *other* than `fault_mutex`, this analysis
   missed a path — re-audit that specific lock's IRQ discipline.

Until fixed: the devbox is fully usable for SSH + non-multithreaded networking; avoid
`curl https://<hostname>` (use an IP, or `http://`).

## Concurrent SSH: do it single-threaded (cooperative multiplexer)

`userspace/sshd` accepts one connection and runs `block_on(protocol::handle_connection(...))`
to completion before the next `accept()` (`main.rs:126-171`). The **whole** session is
already one cooperative `async` future, and its interactive bridge already sets both fds
non-blocking and polls (`protocol.rs:155-208`). So the right fix is a **single-threaded
multi-future executor**, which also keeps sshd a single-threaded process (so it never trips
the `fault_mutex` deadlock class above):

1. Set the listener non-blocking and each accepted fd non-blocking immediately
   (`libakuma::set_nonblocking`, already used by the bridge — fcntl `F_SETFL O_NONBLOCK`,
   `libakuma/src/lib.rs:1020`).
2. Make the `async` socket read actually **yield `Poll::Pending` on `WouldBlock`** instead of
   the current blocking `recv` (`SshStream::read` → `TcpStream::read` → `crate::recv`,
   `libakuma/src/net.rs:251`, which today blocks in-kernel). Today only the bridge tolerates
   `WouldBlock`; the handshake + built-in-shell read loops call `stream.read().await`
   expecting it to block. This is the one real code change.
3. Replace the accept loop's `block_on(one)` with an executor that holds a
   `Vec<Pin<Box<dyn Future<Output=()>>>>` of live sessions: each tick, non-blocking-`accept`
   (push a new `handle_connection` future on success), poll every session future, retain the
   `Pending` ones, and `sleep_ms(1)` only when nothing was ready.

This gives real concurrency (while session A parks on a socket read, B progresses) on one
thread — no `clone`/thread-per-connection, no shared `fault_mutex` contention.

**Thread-per-connection IS possible — but is the *worse* option here.** `libakuma` doesn't
wrap it, but the kernel implements the Linux `clone`/`CLONE_VM` (and `fork`) ABI directly —
that's exactly what curl's `AsynchDNS` thread and llama.cpp's pthreads already use — so sshd
could issue a raw `clone` syscall (or add a thin `libakuma` wrapper) and spawn a handler
thread per connection. **Don't**, for two reasons: (1) it makes sshd a *multithreaded* EL0
process, so until the `fault_mutex` fix above lands it would hit the exact `curl https`
deadlock class (and even after, it adds shared-`fault_mutex` contention the single-threaded
design has none of); (2) the cooperative multiplexer is strictly less machinery — no stacks,
no per-thread teardown, no locking around shared session state. Reach for real threads only
if a session ever needs to do genuinely CPU-bound work that would starve the others.

Related backlog once concurrency lands: the single box-0 rump proxy serializes socket
syscalls, which *may* head-of-line-block truly-simultaneous sessions — re-measure after the
executor is in.

## Touchpoints

- `Cargo.toml`, `crates/akuma-net/Cargo.toml` — the `smoltcp` feature + optional dep.
- `crates/akuma-net/src/lib.rs`, `socket.rs` — module gating + Tier-C stubs + split `init`.
- `src/main.rs` — `mod ssh`/tests gating, built-in-SSH-spawn gating, background-poll gating,
  the `compile_error!` guard (no smoltcp ⇒ must have `userspace-sshd`), crate-level
  `allow(dead_code)` for the rump-only build.
- `src/syscall/{mod,net,poll,fs,term}.rs` — dispatch cfg-else, per-fn gating, and the
  rump-only UnixSocket `sendto`/`recvfrom`/**`sendmsg`** variants (always dispatched) that
  service box 0's rump_server fd-3 channel.
- `src/rump_proxy.rs` — `start_default_stack` (smoltcp-free) + the
  `intercept_box_syscall` fix so a non-socket-family syscall is not proxy-owned merely
  because `args[0]` collides with a rump fd number (the WAITPID hang).
- `scripts/build_size.sh`, `scripts/build_extreme_size.sh`, `scripts/build_devbox.sh`,
  `overlays/devbox/run.sh` — explicit `smoltcp` in the profiles that need it.
