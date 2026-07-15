# Rump stack (sysproxy + fiber backend)

Internals of the NetBSD rump TCP/IP stack as it runs inside Akuma. For how
boxes pick a stack and how packets flow at the top level, see
[`networking.md`](networking.md).

## Components

| Component | Location | Role |
|---|---|---|
| `rump_server` binary | `userspace/rumpkernel/rumpuser/` (feature `rump_server_main`) | The process that owns the NetBSD stack + `/dev/net/tap0`. ~13-14 MB. |
| Kernel sysproxy client | `src/rump_proxy.rs` | Forwards box processes' socket syscalls to the server over fd 3. |
| Rump tap driver | `crates/akuma-net/src/rump_tap.rs`, `rumpcomp_tap.c` | Raw L2 frame path on NIC1 → `/dev/net/tap0`. |
| Fiber backend | `userspace/rumpkernel/` (`threads_fiber` feature) | Cooperative-fiber rumpuser backend. |
| Syscall translation | `crates/akuma-rump/src/syscall_translation.rs` | Maps Linux socket syscalls ↔ rump syscalls; decides what's proxied. |

## The sysproxy architecture

**One `rump_server` process per rump box** owns the NetBSD stack + the tap.
Other in-box processes share it via **rump's sysproxy** (remote-syscall)
mechanism. In the committed design, **Akuma's kernel IS the sysproxy client**:
unmodified binaries' AF_INET syscalls are intercepted and forwarded to the
box's server over a kernel pipe pair on fd 3.

- Transport = kernel pipe pair (no path-based AF_UNIX).
- The proxy is **synchronous on the calling thread**: each forwarded syscall
  blocks the caller until the server replies.
- The server's own pid is excluded from interception (so its own socket calls
  hit the real NetBSD stack natively).

Source: `src/rump_proxy.rs` (`mark_box_rump`, `attach_server`,
`intercept_box_syscall`); `userspace/rumpkernel/docs/RUMP_SYSPROXY.md`.

## The fiber backend (why rump works on one vCPU)

Out of the box, rump spawns ~19 pthread kthreads. On a single-vCPU guest these
spin against each other and a single `curl` over rump took ~63s. The
**cooperative-fiber** `rumpuser` backend (`threads_fiber` feature, now default)
collapses those 19 OS threads into **1 OS thread** running them as fibers.

Result: `clone=0 futex=0`, and `curl` over rump dropped 62.8s → 16.3s → ~1.4s.

Source: `userspace/rumpkernel/docs/FIBER_HANDOFF.md`.

## `start_default_stack` (box 0 / devbox)

`src/rump_proxy.rs:1284`. Only compiled when `rump-default` is on.

1. Bail if NIC1 not ready (`/dev/net/tap0` missing → box 0 stays native).
2. `mark_box_rump(0)`.
3. Spawn `/bin/rump_server --net --fd 3 --log /var/log/box/0/rump_server.log`.
4. `attach_server(0, pid)` — wire fd 3 + handshake in a kthread + publish the
   proxy. Persistent: the server is **never** killed (it is box 0's live stack).

`main` does not block on the handshake (see [`networking.md`](networking.md)).

## The alternative: `stack=rump` herd box

Distinct from `rump-default`. A herd-owned `rump_server` in a **fresh box**
(not box 0) that processes must `join_box` into. This is the path for
arbitrary additional rump boxes on a default-smoltcp build. Status: partly
implemented (the per-box proxy machinery is done; herd's `stack` selector and
bundle generation are Phase 5 / open).

See `archive/RUMP_PLUS_HERD.md`, `userspace/rumpkernel/docs/RUMP_SYSPROXY.md`
"Architecture Questions" Q1-Q4.

## Known limitations (current)

- **Per-syscall round-trip latency.** Every socket syscall crosses the
  kernel→server fd-3 channel and back, serialized. SSH first-connection is
  ~3.4s; `git clone` of a tiny repo took >2min. Open: real readiness waker on
  rump sockets (currently MSG_PEEK poll + 10ms re-poll floor).
- **Single box-0 proxy serializes** socket syscalls — head-of-line blocking
  under truly simultaneous sessions.
- **CPU-bound load starves the rump thread + sshd** on a single core
  (e.g. rustc codegen). Open: raise scheduling weight of the rump proxy thread.
- **Shell pipeline `cmd | head -N` can wedge the VM** at ~99% CPU
  (`[signal] tkill(tid=X, sig=13)` SIGPIPE delivery spins). Workaround:
  redirect to a file.

See `archive/KNOWN_ISSUES.md` §10-11 (both now FIXED: tap-poll busy-spin,
BSP idle busy-yield).

## rump_server flags

| Flag | Effect |
|---|---|
| `--net` | Bring up networking (DHCP on `/dev/net/tap0`). |
| `--fd 3` | Use fd 3 as the sysproxy channel (kernel wire). |
| `--log <path>` | Write the server log to `<path>` (devbox: `/var/log/box/0/rump_server.log`). |

## Background

- `userspace/rumpkernel/docs/RUMP_SYSPROXY.md` — the committed shared-stack design.
- `userspace/rumpkernel/docs/HIJACK_VS_KERNEL_PROXY.md` — the three models; why kernel-side.
- `userspace/rumpkernel/docs/FIBER_HANDOFF.md` — the cooperative-fiber backend.
- `userspace/rumpkernel/docs/ARCHITECTURE_QUESTIONS.md` — box routing decisions.
- `archive/OPTIONAL_SMOLTCP.md` — making this path the only stack in the devbox.
