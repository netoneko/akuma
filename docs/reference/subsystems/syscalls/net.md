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
  `ENOPROTOOPT`. For the 4-byte integer options `getsockopt` writes back an
  `optlen` of `4` and requires the caller's buffer to be at least 4 bytes;
  anything smaller → `EFAULT`.
- **`SO_RCVTIMEO` (20) / `SO_SNDTIMEO` (21) are the exception to that no-op
  rule, and they are the reason the rule is dangerous.** Both are real:
  the value is a 16-byte AArch64 `struct timeval` (`{i64 tv_sec; i64
  tv_usec;}`), stored per socket and consumed by the blocking `recv`/`send`
  wait. Corners that matter:
  - An all-zero timeval means **block indefinitely** (POSIX), not "expire
    immediately". `getsockopt` reports "no timeout" the same way.
  - A negative field, or an `optlen` below 16, is `EINVAL` — not a silent
    accept.
  - `getsockopt` answers these two with 16 bytes and sets `optlen` to 16,
    unlike every other option here. That readback is what lets a client tell
    "honoured" from "dropped", and Rust's `TcpStream::read_timeout()` is
    exactly this call.

  Until 2026-08-17 both fell through the no-op arm: accepted, reported
  successful, and discarded, with no `getsockopt` arm at all — so a client
  that bounded a read to 2 s was not bounded, and could not detect it. It then
  died at the kernel's own undeclared 30 s blocking-read cap instead. Both the
  cap and the missing option are gone; regression
  `socket_timeout_option_roundtrip` in the boot suite. The lesson generalises:
  **a silently-accepted option is indistinguishable from a working one unless
  `getsockopt` can read it back**, so anything added here should be readable.
- **`SO_KEEPALIVE` (9)** arms smoltcp's keep-alive timer via `set_keep_alive`
  at `socket::KEEPALIVE_IDLE_SECS` (7200 s, Linux's `tcp_keepalive_time`).
  smoltcp has one interval rather than Linux's time/intvl/probes triple, so
  that is also the repeat period. A listener's whole pooled backlog is armed,
  so an accepted connection inherits the option instead of losing it.

  Until 2026-08-20 this was the exact failure mode the `SO_RCVTIMEO` note warns
  about, one layer deeper: `set_socket_keepalive` wrote a `KernelSocket`
  field that **nothing in the crate ever read**, and smoltcp's `set_keep_alive`
  was never called from anywhere — so `setsockopt` reported success and Akuma
  emitted no keepalive probe, ever
  ([`../../../archive/DEVBOX_ISSUES.md`](../../../archive/DEVBOX_ISSUES.md)
  Issue 19). What this buys is Akuma *noticing* a peer that vanished without a
  FIN; it does not stop Akuma from tearing down a connection itself, so it is
  not a fix for that issue's 300 s `rc=255`. Regression
  `test_so_keepalive_arms_smoltcp` asserts against smoltcp's own `keep_alive()`
  rather than the local flag — a test against the flag passes in the broken case.
- `connect`: a real in-progress non-blocking connect surfaces
  `EINPROGRESS`, not folded into the generic `neg_errno(e)` path — this is
  the one connect-specific error code callers should expect to see and
  retry-via-`epoll`/`poll` on (see [`../syscalls.md`](../syscalls.md)
  "Blocking vs non-blocking" and [`poll.md`](poll.md)).
- `bind` with port `0` allocates an ephemeral port, for **TCP as well as
  UDP**. Only the UDP arm did until 2026-08-16; the TCP arm stored the literal
  `0`, so the following `connect` handed smoltcp `local_port = 0` and got
  `Unaddressable`. Every client that binds before connecting (`busybox nc`,
  anything setting a source address) failed against a healthy listener.
- `bind` on a TCP socket does **not** record the address, only the port.
  Binding `127.0.0.1:N` and binding `0.0.0.0:N` produce the same listener; a
  smoltcp listener accepts on any local address. There is no way to restrict a
  listener to loopback.

## `connect` state machine and errnos

`connect(2)` is not a single shot at smoltcp: `smoltcp::tcp::Socket::connect`
rejects any socket that is not `Closed`, so the socket layer classifies the
current TCP state first (`connect_step`, `crates/akuma-net/src/socket.rs`).

| Socket state | Result |
|---|---|
| `Closed` / `Listen` / any teardown state | dials — a real SYN |
| `Established` | `0` (success) |
| `SynSent`/`SynReceived`, `O_NONBLOCK` | `EALREADY` |
| `SynSent`/`SynReceived`, blocking | waits for completion, **without re-issuing the SYN** |

`Established` returning *success* rather than POSIX's `EISCONN` is deliberate.
The redial is how the standard non-blocking idiom collects a finished connect —
`connect` → `EINPROGRESS` → poll → `connect` — and hiredis (so `redis-cli`) does
exactly that. Until 2026-08-16 that second call was passed straight to smoltcp,
rejected as `InvalidState`, and reported as `ECONNREFUSED`: **every local client
failed against a listener that was up**, while traffic from outside the VM
worked, which made it look like a loopback bug. It was not.

Failure errnos are distinct, and were not before:

| Errno | Means |
|---|---|
| `ECONNREFUSED` | the socket reached `Closed` — RST, or the stack gave up |
| `ETIMEDOUT` | still half-open at the 10 s deadline |
| `EADDRNOTAVAIL` | smoltcp `Unaddressable` — unroutable remote, or a zero local port |
| `EISCONN` | smoltcp `InvalidState` on a fresh dial |
| `ENETDOWN` | no network stack |

Collapsing all of these into `ECONNREFUSED` is what hid two separate bugs behind
one symptom: "nothing is listening", "the connect never completed" and "the local
address is unusable" were indistinguishable from userspace. Both the state
classification and the errno mapping are pure functions with host tests
(`connect_state_tests`, `crates/akuma-net/src/tests.rs`) — the socket table and
smoltcp are not needed to test them. Background:
[`../../../archive/REDIS_END_TO_END.md`](../../../archive/REDIS_END_TO_END.md) §2.

## No IPv6

`sys_socket` accepts `domain == 2` (`AF_INET`) only; anything else, `AF_INET6`
included, is `EAFNOSUPPORT` (97). smoltcp is built `proto-ipv4` only and there
is not one `Ipv6` identifier in `crates/akuma-net/src/`. This is a missing
feature, not a defect — but it is *loud*: servers that try to bind `::` first
print a scary-looking startup warning and then serve IPv4 normally. See
[`../../../archive/DEVBOX_ISSUES.md`](../../../archive/DEVBOX_ISSUES.md) Issue 9,
which also records that `cargo` cannot reach crates.io because of it while
`curl` can.

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
