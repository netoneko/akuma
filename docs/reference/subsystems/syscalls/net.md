# net syscalls

socket / bind / listen / accept / connect / sendto / recvfrom / sendmsg /
recvmsg / getsockopt / setsockopt / getsockname / getpeername / shutdown /
socketpair. Source: `src/syscall/net.rs`. For the box model, native (smoltcp)
vs. rump stack routing, and `intercept_box_syscall`, see
[`../networking.md`](../networking.md); for rump internals see
[`../rump-stack.md`](../rump-stack.md). This doc covers only the syscall
entry-point surface: which socket syscalls exist, sockaddr/errno validation,
smoltcp-vs-rump differences visible at the syscall boundary, and the
AF_UNIX `socketpair` exclusion.

> **Stability: C (active risk).** Inherits `networking.md`'s grade. Recent
> churn (2026-07-03..07: "devbox uses rumptcp by default", "optional
> smoltcp") is architectural (making smoltcp compile-out-able), not bug-fix
> churn, but that makes this file's `#[cfg(feature = "smoltcp")]` split the
> newest and least battle-tested surface in the tree. The recurring lesson:
> **AF_UNIX socketpairs are not sockets** — every send/recv-family syscall
> has a `fd_is_unix_socket` branch that reroutes to plain pipe read/write
> *before* touching the smoltcp socket table; miss one and a devbox build
> (no smoltcp at all) gets `EBADF` on what should be local IPC.

## Syscall table

| Syscall | nr | Entry point | Gate |
|---|---|---|---|
| `socket` | 198 | `sys_socket` | requires `smoltcp`; else `ENETDOWN` |
| `socketpair` | 199 | `sys_socketpair` | always (AF_UNIX only) |
| `bind` | 200 | `sys_bind` | requires `smoltcp`; else `ENETDOWN` |
| `listen` | 201 | `sys_listen` | requires `smoltcp`; else `ENETDOWN` |
| `accept` | 202 | `sys_accept` | requires `smoltcp`; else `ENETDOWN` |
| `connect` | 203 | `sys_connect` | requires `smoltcp`; else `ENETDOWN` |
| `getsockname` | 204 | `sys_getsockname` | requires `smoltcp`; else `ENETDOWN` |
| `getpeername` | 205 | `sys_getpeername` | requires `smoltcp`; else `ENETDOWN` |
| `sendto` | 206 | `sys_sendto` | always (AF_UNIX path always compiled) |
| `recvfrom` | 207 | `sys_recvfrom` | always (AF_UNIX path always compiled) |
| `setsockopt` | 208 | `sys_setsockopt` | requires `smoltcp`; else `ENETDOWN` |
| `getsockopt` | 209 | `sys_getsockopt` | requires `smoltcp`; else `ENETDOWN` |
| `shutdown` | 210 | `sys_shutdown` | always (no-op stub, returns `0`) |
| `sendmsg` | 211 | `sys_sendmsg` | always (AF_UNIX path always compiled) |
| `recvmsg` | 212 | `sys_recvmsg` | requires `smoltcp` for AF_INET; AF_UNIX path always compiled |
| `accept4` | 242 | `sys_accept4` | requires `smoltcp`; else `ENETDOWN` |
| `resolve_host` (Akuma-specific) | 300 | `sys_resolve_host` | requires `smoltcp`; else `ENETDOWN` |

"Gate" here is the `smoltcp` Cargo feature, **not** one of the `sc-*`
families in [`../syscalls.md`](../syscalls.md)'s split table — `net.rs`
itself is unconditionally in the dispatch match (per that table's "always
(smoltcp **or** rump-routed)" row). `#[cfg(feature = "smoltcp")]` instead
gates individual functions *inside* `net.rs`; the devbox build (rump-only,
`--no-default-features`) compiles the `#[cfg(not(feature = "smoltcp"))]`
twins of `sys_sendto`/`sys_recvfrom`/`sys_sendmsg` and routes everything
else through `net_enetdown()` (`mod.rs:430`, `neg_errno(ENETDOWN)`).

## AF_UNIX socketpair (nr 199) — always-native exclusion

`sys_socketpair` only accepts `domain == AF_UNIX (1)` with `SOCK_STREAM (1)`
or `SOCK_SEQPACKET (5)`; anything else → `EAFNOSUPPORT`. It is backed by two
unidirectional kernel pipes (`px`, `py`) wired crosswise into two
`FileDescriptor::UnixSocket { rx, tx }` entries, and a bad `sv_ptr` (fails
`validate_user_ptr`, 8 bytes for the `[i32; 2]` fd pair) → `EFAULT`, with a
full rollback (`remove_fd` + close both pipe ends both directions) so a
failed copyout never leaks fds or pipe slots.

As `../networking.md` notes, this syscall is **excluded from
`intercept_box_syscall`** — it always runs natively regardless of the
calling process's box/stack assignment, because it's pure local IPC, never
networking. The syscall-boundary detail worth adding: this exclusion isn't
just about socketpair's own dispatch — **every** send/recv-family syscall
(`sendto`, `recvfrom`, `sendmsg`, `recvmsg`, plain `read`/`write` on the fd)
checks `fd_is_unix_socket(fd)` first and reroutes to `super::fs::sys_read`/
`sys_write` on the backing pipe, bypassing the socket table entirely. This
is what makes `std::process::Command`'s exec-status handshake (and
`rustc -C linker=...`, which needs it before it can exec the linker) and the
rump proxy's own fd-3 sysproxy channel (see `../rump-stack.md`) work even in
a rump-only (no-smoltcp) build: `sys_sendmsg`'s `#[cfg(not(smoltcp))]` twin
still handles AF_UNIX (`EBADF` for anything else), and `epoll`/`ppoll`
readiness for a `UnixSocket` fd is keyed off the two pipes' own
readable/writable state (`src/syscall/poll.rs` — see [`poll.md`](poll.md)).
Approximation to be aware of: this is `SOCK_SEQPACKET` **backed by a byte
stream**, so message boundaries are not preserved — fine for libstd's single
fixed-size handshake read, not a conformant SEQPACKET implementation.

## sockaddr / argument validation

- `bind`/`connect`: `len < 16` → `EINVAL` (the `sockaddr_in` struct is 16
  bytes); `!validate_user_ptr(addr_ptr, len)` → `EFAULT`. The copy is bounded
  to `min(len, size_of::<SockAddrIn>())`, so an oversized `len` doesn't
  overrun the kernel-side struct.
- `accept`/`accept4`: `addr_ptr`/`len_ptr`, when non-null, are validated for
  16 and 4 bytes respectively before use; passing `0` for either is legal
  (caller doesn't want the peer address) and skips the copyout.
- `sendto`/`recvfrom`/`sendmsg`/`recvmsg`: the buffer/iovec pointers are
  validated, then copied through a **bounce buffer** (see below) rather than
  touched directly — there is no raw pointer dereference into the socket
  layer.
- `sendmsg`/`recvmsg` read a fixed `MsgHdr` (must fit `validate_user_ptr`
  for `size_of::<MsgHdr>()`), then only ever process **`iovs[0]`** — Akuma
  does not scatter/gather across multiple iovecs on the socket path (a
  short first iovec is not chased into the second). This is sufficient for
  every ported client observed so far (single-iovec DNS/HTTP framing) but is
  a real gap versus POSIX `sendmsg`/`recvmsg` semantics.
- `setsockopt`/`getsockopt`: unrecognized `level`/`optname` pairs are not an
  error — they're logged and treated as a successful no-op (`0`), matching
  the common "ignore options we don't model" strategy rather than
  `ENOPROTOOPT`. `getsockopt` always writes back an `optlen` of `4` and
  requires the caller's buffer to be at least 4 bytes; anything smaller →
  `EFAULT`.
- `connect`: a real in-progress non-blocking connect surfaces
  `EINPROGRESS`, not folded into the generic `neg_errno(e)` path — this is
  the one connect-specific error code callers should expect to see and
  retry-via-`epoll`/`poll` on (see [`../syscalls.md`](../syscalls.md)
  "Blocking vs non-blocking" and [`poll.md`](poll.md)).

## The net bounce buffer

Every `sendto`/`recvfrom`/`sendmsg`/`recvmsg` copies through a **fallible**
kernel bounce buffer (`alloc_net_bounce`, `net.rs:34`), capped at 64 KiB
(`NET_BOUNCE_MAX`). This is a syscall-layer detail worth calling out on its
own: `alloc::vec![0u8; N]` is an infallible allocation, and a 64 KiB (16
contiguous physical pages) request can fail outright under PMM fragmentation
— which used to abort the *entire kernel* via `handle_alloc_error` (see
Background). The fix tries the full size first, then degrades to a single
page (satisfiable whenever any page is free), then returns `ENOMEM` — never
aborting. A short recv/send is legal short-count behavior; callers already
loop. `net_bounce_size_plan` is the pure, unit-tested size-selection
function if you're auditing this path.

## smoltcp vs. rump differences at the syscall boundary

- **smoltcp build** (`feature = "smoltcp"`, box native): sockets are indices
  into a global socket table (`get_socket_from_fd`); TCP/UDP dispatch is by
  `socket::is_udp_socket(idx)`. `EPOLLET`-edge bookkeeping
  (`epoll_on_fd_drained`, see [`poll.md`](poll.md)) is invoked on every
  successful/`EAGAIN` recv path — BoringSSL/bun read one TLS record at a
  time without draining to `EAGAIN`, so the edge must be reset explicitly on
  every read, not just on drain.
- **rump-only build** (no `smoltcp`): there is no local socket table at all.
  A real AF_INET `socket()`/`bind()`/etc. is structurally impossible and
  returns `ENETDOWN` — the rump box's *own* sockets are proxied at a
  different layer (`intercept_box_syscall`, see `../networking.md`), not
  through `net.rs`'s socket table. The one thing `net.rs` still does for a
  rump box is carry the AF_UNIX `UnixSocket` fd (fd 3, the sysproxy channel
  to `rump_server`) through `sendto`/`recvfrom`/`sendmsg` — without those
  AF_UNIX branches staying compiled in a no-smoltcp build, the rump
  handshake banner send fails and box 0's rump stack never comes up (see
  the doc comments at `net.rs:635` and `net.rs:922`).

## Background

- `archive/OPTIONAL_SMOLTCP.md` — making smoltcp compile-time optional; the
  origin of the `#[cfg(feature = "smoltcp")]` split in this file.
- `archive/NET_BOUNCE_OOM_KERNEL_ABORT.md` — the bounce-buffer OOM →
  `brk #1` kernel abort this file's `alloc_net_bounce` fixes.
- `archive/SOCKETSET_EXHAUSTION_FIX.md` — smoltcp `SocketSet` full panic
  (relevant to `EMFILE` from `sys_socket`'s `alloc_socket` failure).
- `archive/BUN_MISSING_SYSCALLS.md`, `archive/SMOLTCP_MIGRATION_SUMMARY.md`,
  `archive/SMOLTCP_MIGRATION_CHALLENGES.md`.
