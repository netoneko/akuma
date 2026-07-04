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
| **devbox (`smoltcp` off)** | ✅ clean, clippy-clean, **1.4 MB** | ✅ **rump default stack + interactive SSH-over-rump + `curl http(s)://host` (200 OK) verified** |

Verified end-to-end on the fully smoltcp-free build: box 0's `rump_server` boots (DHCP
`10.0.2.15`, `SERVING sysproxy on fd 3`), the kernel handshake completes (`box=0 proxy
ready`), herd's userspace `sshd` binds/accepts over rump, an **interactive SSH session
runs commands** (`echo`, `uname -a`, `ls /`), and over the rump stack (DNS + TCP + HTTP(S),
incl. curl's multithreaded resolver path) both **`curl http://example.com` and
`curl https://example.com` return `200`** — no smoltcp compiled in. HTTPS verifies peer
certs against the Mozilla CA bundle staged by `overlays/devbox/bootstrap.sh` step 5 (apk
`ca-certificates-bundle`).



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

- ~~**One-shot `ssh host <cmd>` doesn't spawn the child**~~ — **FIXED.** `handle_message`
  only recognized the `shell` channel-request type; `exec` fell through with no reply and
  no spawn. `run_exec_session` (`userspace/sshd/src/protocol.rs`) now parses the command
  string out of the same request and spawns `<shell> -c <cmd>` through `bridge_process`.
- ~~**Concurrent SSH sessions don't work**~~ — **FIXED**, see the Concurrent SSH section
  below (now retitled) and [`userspace/sshd/docs/FLOW.md`](../userspace/sshd/docs/FLOW.md)
  for the full before/after diagram.

## Concurrency: `curl https://host` — FIXED (clone child got a bogus TTBR0)

This was the freeze that dogfooding surfaced. **Fixed 2026-07-04.** Recorded here so the
next session doesn't re-walk the wrong tree: the *first* hypothesis (a `fault_mutex`
IRQ-safety deadlock under CLONE_VM) was a red herring; the real bug is in `clone_thread`.

**Symptom.** `curl http://<IP>` worked, single-threaded `curl http://<host>` (DNS-over-rump)
worked, `curl https://<IP>` (TLS, no DNS) worked. `curl https://<host>` — and any
multithreaded process whose first `clone(CLONE_THREAD)` ran right after an `execve` —
silently hung the whole box: heartbeat (the main thread's idle loop) stopped advancing, the
shell never returned, no panic/fault log. Pure logic bug, 100% reproducible.

**Root cause (`crates/akuma-exec/src/process/mod.rs::clone_thread`).** A new CLONE_THREAD
sibling must share the leader's address space. clone_thread built the child's saved
register context (`child_ctx`) from `get_saved_user_context(parent)`, whose `ttbr0` field
is read out of `THREAD_CONTEXTS[parent].ttbr0` — a value that is only refreshed when the
SGI context-switch code switches *away* from that thread. A freshly-`execve`'d process
(curl) activates a brand-new address space (new TTBR0 written to `TTBR0_EL1` directly via
`activate()`) **without** the SGI switch path ever running for it, so
`THREAD_CONTEXTS[parent].ttbr0` still holds a stale/bogus value. clone_thread copied that
bogus value into the child, then `update_thread_context` wrote it to the child's kernel
context. The fix:

```rust
// captured straight off the still-live address space, before `shared_as` is moved:
let shared_ttbr0 = parent.address_space.ttbr0();
...
let mut child_ctx = parent_ctx;
child_ctx.x0 = 0;
child_ctx.sp = stack;
child_ctx.tpidr = tls;
child_ctx.spsr = 0;
child_ctx.ttbr0 = shared_ttbr0;   // OVERRIDE the stale inherited value
```

**Mechanism of the hang (confirmed via the SGI handler, not guessed).** Tracing the
scheduler: `clone_thread` correctly created the child (TID 13), marked it READY, and
returned; the parent (TID 12) then yielded voluntarily; `schedule_indices` correctly chose
`12 → 13`; the SGI switch code loaded the child's **`new_ttbr0 = 0x5000062_eda000`** (a
~90 PB "physical address" — obvious garbage; RAM is at `0x4000_0000`) into `TTBR0_EL1`,
flushed the TLB, and ERET'd to the child's user PC. With no valid user page table, the
first instruction fetch faulted in a way that left the CPU wedged with IRQs masked, so the
timer SGI could no longer fire and no other thread (incl. the heartbeat) ever ran again.
Once the child got the parent's *real* TTBR0, `curl http://example.com` returned `200`
immediately and `curl https://host` reached the TLS step.

**Why the `fault_mutex` hypothesis was wrong (and what was nonetheless done).** The earlier
analysis fingered `Process::fault_mutex` — the one shared spinlock on the demand-paging
fault path acquired *without* `with_irqs_disabled` — and proposed wrapping its
`fault_slot_acquire`/`fault_slot_release` critical sections in `with_irqs_disabled`. That
IRQ-safety change was applied (`crates/akuma-exec/src/process/children.rs`): it is correct
hygiene (a holder could be preempted mid-section on a single CPU and self-deadlock a nested
contender), it's kept, and the host regression test (`test_fault_mutex_insert_remove`,
 `src/process_tests.rs`) still passes — but it does **not** fix `curl https`, because the
freeze was never a fault-path spin. The decisive evidence that ruled it out: an SGI-handler
sampler logged the preempted thread's PC (`ELR_EL1`) across the freeze and found the box
*mostly idle* (threads parked in `yield_now`/`trigger_sgi`), with **zero** EL1 sync
exceptions — i.e. a wait/freeze, not a spin and not a fault loop. The actual spin site was
the SGI switch code itself, mid-`ERET`, after loading the bogus TTBR0. **Lesson: trust
`ELR_EL1` sampled from the SGI handler over a gdbstub PC snapshot** — under HVF the gdbstub
consistently misreports the PC as the exception-vector entry (and `ESR`/`ELR`/`FAR` read
back equal to the PC), so only the general registers (notably `SP`, `LR`) are trustworthy.
For a reliable PC snapshot, force `HVF=0` (TCG) for the repro.


## `fork_process` stale TTBR0 — FIXED (same bug class, fork path was never patched)

**Fixed 2026-07-04.** The `clone_thread` TTBR0 fix above was only ever applied to
`clone_thread` — the identical code path in `fork_process` was left with the stale
inheritance. This made every fork+execve on the rump-only devbox wedge the VM:

**Symptom.** `nslookup github.com` and `curl http://github.com` worked (no fork), but
`git clone`, `wget`, and any program that forks+execves a child silently hung the whole
VM — no `ec=0x20` in TCG, no panic, no heartbeat, just a dead SSH session. Under HVF the
git case additionally flooded `qemu-system-aarch64: 0x401b2550: unhandled exception ec=0x20`
(the SGI scheduler's `isb` right after `msr TTBR0_EL1` + `tlbi vmalle1`).

**Root cause (`crates/akuma-exec/src/process/mod.rs::fork_process`).** Same as clone_thread:
`get_saved_user_context(parent_tid)` reads `THREAD_CONTEXTS[parent].ttbr0`, which is stale
for any thread that execve'd/mmap'd since its last context-switch-out. fork_process copied
that stale value into `child_ctx` and never overrode it — even though the child gets a
**fresh, independent** address space whose ttbr0 is `new_proc.address_space.ttbr0()`, not
the parent's. The one-line fix mirrors clone_thread's:

```rust
// After child_ctx = parent_ctx + x0=0 + spsr=0 + sp override:
child_ctx.ttbr0 = new_proc.address_space.ttbr0();
```

**Why curl worked but git/wget didn't.** curl (single-threaded, `AsynchDNS` via
`clone_thread`) was covered by the earlier fix. git and wget fork+execve to run
subprocesses (git-remote-https, busybox wget), and the fork child inherited the stale
ttbr0. The DNS path itself (musl `__res_msend`: `sendto` + `recvmsg` over the rump proxy)
was never broken — `nslookup` proved it. The "DNS error" was the SSH session dying when
the VM wedged during the fork, before any DNS output was produced.

**Regression test.** `test_fork_child_context_ttbr0_not_stale` in `src/process_tests.rs` —
creates two independent `UserAddressSpace`s and verifies the override invariant: the
child's context ttbr0 must equal `child_as.ttbr0()`, not the inherited parent value.


## Concurrent SSH — FIXED (cooperative multiplexer)

**Fixed 2026-07-04.** Full before/after diagram, the `bridge_process`/`fail_spawn`
mechanics, and the wire-format test coverage: see
[`userspace/sshd/docs/FLOW.md`](../userspace/sshd/docs/FLOW.md). Summary of what actually
shipped, and where it deviated from the plan originally sketched in this section:

1. **Listener + accepted fds non-blocking, `TcpListener::try_accept()`, and the accept loop
   replaced with a `Vec<Pin<Box<dyn Future<Output=()>>>>` executor** — as planned.
2. **`SshStream::read`/`write` yield `Poll::Pending` on `WouldBlock`** — as planned, plus a
   non-suspending `SshStream::try_read()` for `bridge_process`'s own manual stdout/stdin
   interleaving (it can't afford to suspend the *whole* future mid-tick just to wait on one
   direction).
3. **A blocker the plan didn't anticipate: `fcntl(F_SETFL, O_NONBLOCK)` on a rump socket
   was hard `EOPNOTSUPP`'d** (`src/rump_proxy.rs`, from the WAITPID-collision fix below) —
   so step 1 alone crash-looped `sshd` the instant it tried to go non-blocking. Fixed by
   implementing real `F_SETFL`/`F_GETFL` handling for rump fds instead of the blanket
   rejection (`rump_fcntl` in `rump_proxy.rs`).
4. **A second blocker: every `bridge_process` idle loop called `sleep_ms` (a blocking
   `NANOSLEEP` syscall) with no `.await` on it.** Rust only suspends an `async fn` at an
   explicit `.await` point, so this loop never actually returned `Poll::Pending` — the first
   session to go idle monopolized the executor's one OS thread for its entire lifetime,
   starving every other connection's `accept`/poll until that session's shell exited.
   Fixed with a proper one-shot `yield_now()` (`userspace/sshd/src/main.rs`) in place of the
   blocking sleep.
5. **A third blocker: `sshd`'s own `TerminalState` was inherited by every `spawn_pty`
   child**, so two sessions' shells shared one `input_waker` slot — a stdin wakeup for
   session A's shell could get delivered to session B's parked reader instead. Fixed in
   `crates/akuma-exec/src/process/spawn.rs`: a `pty` spawn no longer inherits the caller's
   terminal state (real Unix semantics too — a new pty is a new session, not a share of the
   spawner's).

This gives real concurrency (while session A parks on a socket read or sits idle at its
shell prompt, B progresses) on one thread — no `clone`/thread-per-connection, no shared
`fault_mutex` contention, and (per the built-in-shell removal) no fallback shell either:
`sshd` always spawns a real shell now (`config::DEFAULT_SHELL` = busybox's `/bin/sh`); a
spawn failure ends the session with an error message instead of falling back.

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

Related backlog, still open: the single box-0 rump proxy serializes socket syscalls, which
*may* head-of-line-block truly-simultaneous sessions under heavy load — not yet re-measured
now that the executor is in.

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
- `overlays/devbox/bootstrap.sh` — step 5 stages the TLS CA trust bundle (apk
  `ca-certificates-bundle`, dep-free) into the image so `curl https` (mbedTLS) verifies
  peers. Skip with `DEVBOX_CA_CERTS=false` (e.g. offline builds).
- `crates/akuma-exec/src/process/mod.rs::clone_thread` — the `curl https` freeze fix: a
  CLONE_THREAD child's kernel-context `ttbr0` is set to the parent's *live*
  `address_space.ttbr0()` (captured before `shared_as` is moved), not the stale value in
  `get_saved_user_context(parent)`. `crates/akuma-exec/src/process/mod.rs::fork_process` — the
  same ttbr0 override for the fork path (child gets its own fresh `address_space.ttbr0()`).
  `crates/akuma-exec/src/process/children.rs` —
  `fault_mutex` IRQ-safety in `fault_slot_acquire`/`fault_slot_release` (correct hygiene;
  not the curl freeze).
- Concurrent SSH (see that section above) — `userspace/sshd/src/main.rs` (the executor +
  `yield_now`), `protocol.rs` (`SshStream::try_read`, `run_exec_session`, `fail_spawn`,
  `bridge_process`), `userspace/libakuma/src/net.rs` (`TcpListener::try_accept`/
  `set_nonblocking`), `src/rump_proxy.rs::rump_fcntl`, and
  `crates/akuma-exec/src/process/spawn.rs` (pty spawns get a fresh `TerminalState`). The
  built-in fallback shell (`userspace/sshd/src/shell/`) was removed entirely; `sshd` now
  always spawns a real shell (`config::DEFAULT_SHELL` = busybox's `/bin/sh`). Its wire-format
  helpers (`read_string`/`read_u32`/packet framing/`SimpleRng`) now come from the shared,
  tested `crates/akuma-ssh-crypto` instead of a duplicated copy. `/proc/<pid>/stat`
  (`src/vfs/proc.rs`) was added so `ps`/`top` can list processes at all — they parse that
  file, not `/proc/<pid>/status`. `scripts/populate_disk.sh`'s base symlink step no longer
  points `sh`/`cat`/`echo`/etc. at a `busybox.static` that may not exist.
