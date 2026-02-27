# Curl Networking Fixes

Alpine's `curl` (8.17.0, via `apk add curl`) required seven kernel fixes
before it could successfully make HTTP requests. This document records the
root causes found during investigation and the fixes applied.

curl is now working for HTTP. HTTPS requires TLS which depends on additional
kernel support not yet implemented.

## Symptoms

**Phase 1 -- DNS failure:**

```
akuma:/> curl -Lv https://ifconfig.me/ip
* Could not resolve host: ifconfig.me
* shutting down connection #0
```

Kernel log showed unknown syscall 19 (`eventfd2`), IPv6 socket creation
failing with wrong errno, three UDP sockets opened to the DNS server
(10.0.2.3:53) all timing out, and c-ares DNS resolution failing after retries.

**Phase 2 -- DNS resolved but empty reply (after fixes 1-6):**

```
akuma:/> curl -Lv http://ifconfig.me/ip
* Host ifconfig.me:80 was resolved.
*   Trying 34.160.111.145:80...
* Established connection ...
* Empty reply from server
curl: (52) Empty reply from server
```

Non-blocking `connect()` returned `EINPROGRESS`, but `ppoll` immediately
reported the socket as writable before the TCP handshake completed, causing
curl to send the HTTP request into a half-open socket.

## Root Causes and Fixes

### 1. `recvmsg` did not zero `msg_controllen`

**Files:** `src/syscall.rs` (`sys_recvmsg`)

curl's DNS library (c-ares 1.34.6) enables `IP_RECVTTL` via `setsockopt`
(stubbed to return success). It then calls `recvmsg()` with a `msg_control`
buffer, expecting the kernel to fill in ancillary data and set `msg_controllen`
to the actual length received. The kernel set `msg_flags = 0` but left
`msg_controllen` untouched, so c-ares iterated over garbage cmsg headers in the
control buffer and mishandled DNS responses.

**Fix:** Set `msg.msg_controllen = 0` alongside `msg.msg_flags = 0` in both
the UDP and TCP return paths of `sys_recvmsg`.

### 2. Non-blocking sockets not supported

**Files:** `src/syscall.rs`, `src/socket.rs`, `src/process.rs`

`fcntl(F_GETFL)` always returned 0 and `fcntl(F_SETFL)` was a no-op.
`SOCK_NONBLOCK` (0x800) was masked off in `sys_socket` but never stored.
All socket operations (`connect`, `send`, `recv`) blocked unconditionally,
breaking c-ares's event-driven DNS model. c-ares sets its sockets non-blocking
and expects `EAGAIN` when no data is available; instead it got blocked for up to
10 seconds per `recv` call.

**Fix (three parts):**

- **Per-fd flag storage:** Added `nonblock_fds: Spinlock<BTreeSet<u32>>` to the
  `Process` struct (mirrors the existing `cloexec_fds` pattern), with
  `set_nonblock`, `clear_nonblock`, and `is_nonblock` helpers. Cloned on fork,
  cleared on close.

- **fcntl and sys_socket:** `F_GETFL` returns `O_NONBLOCK` (0x800) when the fd
  is marked non-blocking. `F_SETFL` sets or clears the flag. `sys_socket` now
  applies `SOCK_NONBLOCK` when present in the type argument.

- **Socket operations:** `socket_connect`, `socket_send`, `socket_recv`, and
  `socket_recv_udp` all accept a `nonblock: bool` parameter. When true:
  - `connect` initiates the TCP handshake and returns `EINPROGRESS` immediately
  - `send` returns `EAGAIN` if the socket can't send
  - `recv` / `recv_udp` return `EAGAIN` if no data is available

  All syscall entry points (`sys_connect`, `sys_sendto`, `sys_recvfrom`,
  `sys_sendmsg`, `sys_recvmsg`, `sys_read`, `sys_write`) look up the
  non-blocking flag and pass it through.

### 3. `eventfd2` (syscall 19) not implemented

**Files:** `src/syscall.rs`, `src/process.rs`

c-ares uses `eventfd2(0, EFD_NONBLOCK | EFD_CLOEXEC)` for its internal event
loop. Without it, c-ares falls back to `pipe2`, but the missing syscall
returned `ENOSYS` and logged noise.

**Fix:** Implemented `eventfd2` following the existing pipe infrastructure
pattern:

- `KernelEventFd` struct with `counter: u64`, `flags: u32`, and
  `reader_thread: Option<usize>`, stored in a global
  `EVENTFDS: Spinlock<BTreeMap<u32, KernelEventFd>>` table.
- `EventFd(u32)` variant added to the `FileDescriptor` enum.
- `read()`: returns the 8-byte counter value (reset to 0, or decrement by 1
  for `EFD_SEMAPHORE`). Blocks if counter is 0 unless `EFD_NONBLOCK` is set,
  in which case returns `EAGAIN`.
- `write()`: adds the 8-byte written value to the counter; wakes blocked
  reader.
- `ppoll`: reports `POLLIN` when counter > 0, `POLLOUT` always.
- Cleanup on `close`, `close_cloexec_fds`, and `cleanup_process_fds`.
- Flags: `EFD_CLOEXEC` (0x80000), `EFD_NONBLOCK` (0x800), `EFD_SEMAPHORE` (1).

### 4. `read()`/`write()` on UDP sockets called TCP-only functions

**Files:** `src/syscall.rs`

The `Socket(idx)` branch in `sys_read` called `socket_recv()` and in
`sys_write` called `socket_send()`, both of which only handle TCP
(`SocketType::Stream`). On a UDP socket these returned `EBADF`. While c-ares
uses `send()`->`sendto` and `recv()`->`recvfrom` (which handled UDP correctly),
any code path using `read()`/`write()` on a connected UDP socket would fail.

**Fix:** Both branches now check `socket::is_udp_socket(idx)` and dispatch to
`socket_recv_udp` / `socket_send_udp` for datagrams. For `write()` on an
unconnected UDP socket, returns `EDESTADDRREQ`.

### 5. `sys_socket` returned wrong errno for IPv6

**Files:** `src/syscall.rs`

`socket(AF_INET6, ...)` returned `!0u64` (-1 = `EPERM`) instead of
`-EAFNOSUPPORT` (-97). curl could misinterpret `EPERM` as a permissions
problem rather than "IPv6 not available" and fail to fall back to IPv4.

**Fix:** Added `const EAFNOSUPPORT: u64 = (-97i64) as u64` and return it from
`sys_socket` when the domain is unsupported.

### 6. `getsockopt` stub did not write to userspace

**Files:** `src/syscall.rs`

The `getsockopt` dispatch returned 0 (success) without writing anything to
the value buffer. `getsockopt(fd, SOL_SOCKET, SO_ERROR, &val, &len)` left
garbage in `val`, so curl could think a connection had an error when it didn't.

**Fix:** Implemented `sys_getsockopt` that writes `0i32` to the option value
buffer and sets the length pointer to 4. All socket options currently report
no error / zero value, which is correct for a kernel with no error queuing.

### 7. `ppoll`/`pselect6` reported TCP sockets as always writable

**Files:** `src/syscall.rs`

Both `sys_ppoll` and `sys_pselect6` unconditionally set `POLLOUT` / write-ready
for TCP sockets without checking the socket state. After a non-blocking
`connect()` returns `EINPROGRESS`, curl polls the socket waiting for `POLLOUT`
to signal that the TCP handshake has completed. Since POLLOUT was always
reported, curl immediately thought the connection was established and sent
its HTTP request into a socket still in `SynSent` state. The data was lost
and the server never saw the request, resulting in "Empty reply from server."

**Fix:** Added `socket_can_send_tcp()` helper that checks smoltcp's
`tcp::Socket::can_send()`, which only returns true once the socket reaches
`Established` state and has transmit buffer space. Both `sys_ppoll` and
`sys_pselect6` now use this check instead of unconditionally reporting TCP
sockets as writable.

## Syscall Coverage After Fixes

| Syscall | Number | Status |
|---------|--------|--------|
| `eventfd2` | 19 | Implemented |
| `socket(AF_INET6)` | 198 | Returns `EAFNOSUPPORT` |
| `fcntl(F_GETFL)` | 25 | Returns `O_NONBLOCK` flag |
| `fcntl(F_SETFL)` | 25 | Sets/clears `O_NONBLOCK` |
| `getsockopt` | 209 | Writes zero value to buffer |
| `setsockopt` | 208 | Stub (returns success) |
| `recvmsg` | 212 | Zeros `msg_controllen` |
| `ppoll` | 73 | Checks `can_send()` for TCP POLLOUT |
| `pselect6` | 72 | Checks `can_send()` for TCP write-ready |
