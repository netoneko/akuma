# AF_UNIX on Akuma: what exists, what is missing, and how to build the rest (2026-08-23)

**Status: Phases 0-3 IMPLEMENTED and verified on three build targets
(2026-08-23). Phase 4 (`SCM_RIGHTS`) and Phase 5 (introspection, size gate)
are open.** Sections 1-8 below are the original audit and plan, kept verbatim;
what actually landed, what the plan got wrong, and the five defects the probe
found are in § 0 immediately below.

This document was an audit of the AF_UNIX surface Akuma had (a `socketpair`
shim and nothing else), a gap list against what real workloads call, a phased
implementation plan with the host-testable seam named, and the two verification
harnesses the plan was written against: `cargo test -p akuma-net` for the pure
state machine and a fifth `userspace/nettest` probe — `nettest-unix` — that runs
the same binary on Akuma and on Linux so every claim has a control arm.

---

## 0. Outcome

### What landed

| Piece | Where |
|---|---|
> **Moved 2026-08-30.** Everything below that says `akuma-net/src/unix.rs` /
> `unix_tests.rs` is now `crates/akuma-net-unix/src/lib.rs` / `tests.rs`, and
> the import is `akuma_net_unix::` rather than `akuma_net::unix::`. Nothing
> about the design changed — the module was lifted whole, its only coupling
> being `libc_errno`, which now comes from `akuma_primitives::errno` directly.
> Rationale: `docs/archive/AKUMA_NET_SPLIT.md` §5.1 extraction A.

| The pure state machine — codec, name table, rendezvous, framing, shutdown, credentials, datagram resolution | `crates/akuma-net-unix/src/lib.rs` (was `akuma-net/src/unix.rs` until 2026-08-30) |
| 101 host tests for it | `crates/akuma-net-unix/src/tests.rs` (`cargo test -p akuma-net-unix`: 90) |
| Kernel half — the one table, user-pointer copies, pipes, parking | `src/syscall/unixsock.rs` |
| `socket`/`bind`/`listen`/`accept`/`accept4`/`connect`/`getsockname`/`getpeername`/`shutdown`/`getsockopt`/`setsockopt`/`recvmsg` dispatched **unconditionally** via `net::dispatch_*` | `src/syscall/net.rs`, `src/syscall/mod.rs` |
| `S_IFSOCK` + `EXT2_FT_SOCK` + `create_socket_node` | `crates/akuma-ext2/src/ext2.rs`, `crates/akuma-vfs/src/types.rs`, `src/vfs/mod.rs` (5 host tests) |
| Listener readiness (`EPOLLIN` on a non-empty backlog) | `src/syscall/poll.rs` |
| AF_UNIX bypasses the rump proxy entirely | `src/rump_proxy.rs` |
| 10 boot self-tests | `src/process_tests.rs` |
| The probe | `userspace/nettest/rust/unixsock/` → `nettest-unix` |

Phases 0-3 of § 4 are done: the iovec/framing/shutdown/`SO_TYPE`/`MSG_DONTWAIT`
fixes (Phase 0), the table + abstract namespace (Phase 1), the filesystem
namespace with real `S_IFSOCK` nodes (Phase 2), and `SOCK_DGRAM` with framing
(Phase 3). Phase 4 (`SCM_RIGHTS`; `SO_PEERCRED` itself is done) and Phase 5
(`/proc/net/unix`, the `sc-unix-socket` size gate) are not.

### Verification

`nettest-unix` runs 13 modes. **Linux (the control arm), the default `--release`
build at `SMP=4`, and the rump-only devbox all agree on 13 of 13** — the only
difference being `passfd`, which is `OK` on Linux and `UNSUPPORTED` in the guest
because `SCM_RIGHTS` is Phase 4. Boot suite: 309 `PASSED`, 0 `FAILED` at
`SMP=1` and `SMP=4`. Host: `cargo test` green across the workspace; `cargo
clippy --release` reports zero warnings on both feature sets.

### The five defects the probe found

Each was found by *diffing against the Linux control arm*, not by a test written
in advance — which is the argument for § 6's design:

1. **`SHUT_RD` destroyed already-received data.** Linux returns the buffered
   bytes and only then reads as EOF; Akuma returned 0 immediately, silently
   throwing away a complete message the peer had successfully sent. The probe's
   own assertion was wrong in the same direction, and the Linux arm corrected
   both.
2. **`SO_PEERCRED` reported pid 0 on a `socketpair`.** `UnixTable::pair` never
   set `peer_creds`, so a daemon identifying its peer by pid read 0 for
   everyone.
3. **`bind` created a regular file, not a socket node.** `stat` reported
   `mode=0o100644, S_ISSOCK=false`, so a client that checks `S_ISSOCK` before
   connecting — the normal thing to do — refused to talk to a working socket.
   This is G7, and it is why Phase 2 got done rather than deferred.
4. **AF_UNIX did not work inside a `stack=rump` box at all.** `rump_proxy`
   intercepts socket-family syscalls and forwards them to NetBSD, whose sysproxy
   has no AF_UNIX, so `socket(AF_UNIX)` returned `EAFNOSUPPORT`. Fixed by
   letting AF_UNIX fall through to the native path — a unix socket has no wire
   and cannot leak onto a network stack, so this does not weaken the proxy's
   hard-isolation guarantee. `socketpair` was *already* excluded from the proxy
   on precisely this reasoning; the rest of the family had simply never existed.
5. **`recvmsg`/`getsockopt`/`setsockopt` answered `ENETDOWN` on the rump build.**
   § 4's Phase 1 listed the syscalls to un-gate and these three were not among
   them, so a unix socket got a *network* error for having no network.

### What the plan got wrong

- **The rump-only target did not compile, and had not for some time.** § 7 step 5
  says to verify on it; that was impossible at `dbe9b998`. Four
  `#[cfg(feature = "smoltcp")]` gates had been lost — the most consequential in
  `akuma-net`'s `lib.rs`, where a doc comment and a `pub use` inserted between
  the attribute and `pub mod smoltcp_net` silently moved the gate onto the
  re-export. `scripts/build_devbox.sh` failed with 40+ "unlinked crate
  `smoltcp`" errors. All four are fixed here; none was AF_UNIX-related. The
  class of mistake is worth remembering: **an attribute attaches to the next
  item, and a doc comment is an item's attribute**, so anything inserted between
  a `#[cfg]` and its target relocates the gate instead of failing at the edit.
- **§ 3.2 said to keep a `Record` per stream write.** That made the plain
  `SOCK_STREAM` path heap-allocate on its first write (a `VecDeque` push) for
  metadata no reader consults. Replaced with a `pending_bytes: usize` counter —
  which also has to exist for *correctness*, not just speed: without it, bytes
  written **before** an `SCM_RIGHTS` message leave no trace, so a reader draining
  only those bytes would pop the fd-carrying record and receive descriptors it
  had not been told about.
- **§ 4 Phase 1 under-counted the syscalls to un-gate** — see defect 5.
- **§ 3.4's `SCM_RIGHTS` teardown accounting exists but is unreachable.** The
  `Record::anc_fds` plumbing, `detach_channel`'s return value and the
  `unix_channel_detach` call site are all in place, with `debug_assert!`s that
  fire if descriptors ever appear before the close path is wired. Phase 4 is a
  smaller job than § 4 implies because of it.

### Known limitations, stated rather than hidden

- **`SO_PEERCRED.uid`/`.gid` are 0 for every process**, because this kernel has
  no per-process uid (`getuid` hardcodes 0). Anything security-relevant must
  **not** gate on the uid; `pid` is real. The capture path is written so that
  adding real uids is a one-line change.
- **`SCM_RIGHTS` is not implemented** — `recvmsg` reports no ancillary data.
- **`DirEntry` has no socket flag**, so a directory *listing* does not
  distinguish a socket node; `stat` does, and that is what `ls -l` and every
  client actually use.
- **No `sc-unix-socket` feature yet**, so the `extreme-size` 4 MB floor pays for
  the whole family. Phase 5.

> ### The one-line summary
>
> `socket(AF_UNIX, …)` returns **EAFNOSUPPORT** — there is no AF_UNIX socket
> object in the kernel at all. What exists is `socketpair(2)`, which hands back
> two `FileDescriptor::UnixSocket { rx, tx }` entries wired to **two kernel
> pipes**, plus special-case arms in eight syscalls that route such an fd to
> `pipe_read`/`pipe_write`. There is no name, no listener, no accept queue, no
> ancillary data, and no `SOCK_DGRAM`. Everything a *server* needs is absent.

---

## 1. What exists today

### 1.1 The whole of it: `socketpair` over two pipes

`src/syscall/net.rs:137` `sys_socketpair` accepts `domain == 1` (AF_UNIX) with
`SOCK_STREAM` or `SOCK_SEQPACKET`, creates two unidirectional pipes, and
allocates two fds:

```rust
let px = super::pipe::pipe_create();
let py = super::pipe::pipe_create();
let fd0 = proc.alloc_fd(FileDescriptor::UnixSocket { rx: px, tx: py });
let fd1 = proc.alloc_fd(FileDescriptor::UnixSocket { rx: py, tx: px });
```

The descriptor variant (`crates/akuma-exec/src/process/types.rs:161`) is
literally a pair of pipe ids:

```rust
UnixSocket { rx: u32, tx: u32 },
```

That is the entire AF_UNIX object model. It carries no address, no type, no
peer identity, no option state, and no queue of pending connections.

### 1.2 Why it exists

Two consumers, both accidental:

1. **Rust libstd's spawn handshake.** `std::process::Command` builds a
   `SOCK_SEQPACKET` socketpair to relay a failed child `exec`'s errno back to
   the parent, and reads it with `recvmsg`. `rustc` does this before exec'ing
   the linker, so without `socketpair` a link fails with
   `could not exec the linker: Function not implemented`. The follow-on fix —
   routing `UnixSocket` fds through the *socket* syscalls, not just
   `read`/`write` — is recorded in the boot self-test at
   `src/process_tests.rs:10440`.
2. **The rump sysproxy channel.** `src/rump_proxy.rs:251` and `:1469` install a
   `UnixSocket` at **fd 3** of the box-0 `rump_server`, which answers every
   proxied syscall over it. That is why `SENDTO`/`RECVFROM`/`SENDMSG`/`SHUTDOWN`
   are dispatched **ungated** in `src/syscall/mod.rs:718-745` while `BIND`,
   `LISTEN`, `ACCEPT`, `CONNECT`, `GETSOCKNAME`, `GETPEERNAME`, `SETSOCKOPT`,
   `GETSOCKOPT` and `RECVMSG` fall to `net_enetdown()` on a rump-only build.

Any AF_UNIX work must keep both of these working byte-for-byte. The rump
handshake in particular depends on the *coalescing* behaviour of the rump-only
`sys_sendmsg` (`src/syscall/net.rs:976`): all iovecs are concatenated into one
pipe write so the client wakes exactly once with a complete frame. That is a
latency fix (docs/archive/RUMP_SYSPROXY_LATENCY_FIX.md §3q), not an accident.

### 1.3 The eight syscalls that know about `UnixSocket`

| Syscall | Site | Behaviour |
|---|---|---|
| `read` | `src/syscall/fs.rs:688` | `pipe_read(rx)`, honours `O_NONBLOCK`, re-arms the epoll edge |
| `write` | `src/syscall/fs.rs:1138` | `pipe_write(tx)` |
| `dup` / `dup3` / `F_DUPFD` | `fs.rs:1470`, `:1505`, `:1539`, `:1844`, `:1906`, `:2480` | `pipe_clone_ref` both directions |
| `sendto` | `net.rs:426` (+ `:600` rump-only) | `sys_write` on the fd |
| `recvfrom` | `net.rs:514` (+ `:608` rump-only) | `sys_read` on the fd |
| `sendmsg` | `net.rs:906` (smoltcp) / `:976` (rump-only) | first iovec only / all iovecs coalesced |
| `recvmsg` | `net.rs:1058` | first iovec only; zeroes `msg_controllen` and `msg_flags` |
| epoll/poll | `src/syscall/poll.rs:586` | `EPOLLIN` from `pipe_can_read(rx)`, `EPOLLOUT` from `pipe_can_write(tx)` |
| `/proc/<pid>/fd` | `src/vfs/proc.rs:224` | renders as `socket:[<rx pipe id>]` |

Predicate: `fd_is_unix_socket` (`net.rs:1164`).

### 1.4 The existing test coverage

Five boot self-tests, all in `src/process_tests.rs`, registered at `:480-484`:

```
test_socketpair_not_enosys                      :10356
test_socketpair_domain_rejected                 :10367
test_socketpair_bidirectional                   :10379
test_socketpair_close_refcount                  :10411
test_socketpair_recv_send_via_socket_syscalls   :10440
```

They drive `handle_syscall` directly with `BYPASS_VALIDATION` set. There are
**zero** host tests: `crates/akuma-net/src/tests.rs` has 37 tests, all
AF_INET address/errno/DNS. Nothing about AF_UNIX is host-testable today because
the whole implementation lives in kernel-only `src/syscall/`.

---

## 2. The gap list

Ordered by how often a real program trips over it.

### G1 — `socket(AF_UNIX, …)` does not exist

`sys_socket` (`net.rs:110`) rejects anything but `domain == 2`:

```rust
if domain != 2 || (base_type != 1 && base_type != 2) {
    return EAFNOSUPPORT;
}
```

So **every** program that builds a unix socket the normal way — bind a path,
listen, accept — fails at the first syscall. This is the root gap; G2-G5 are
only reachable once it is closed.

### G2 — no filesystem namespace: `bind`/`listen`/`accept`/`connect` by path

There is no mapping from a pathname to a socket. Consequences:

- No unix-socket **server** can run: nginx/php-fpm, postgres, mysql, dockerd,
  containerd, dbus, X11, `sshd`'s `ControlMaster` and agent forwarding
  (`SSH_AUTH_SOCK`), `redis --unixsocket`.
- `/dev/log` cannot exist, so musl's `syslog(3)` — a `SOCK_DGRAM` connect to
  `/dev/log` — silently drops every log line. busybox `syslogd` cannot bind it.

### G3 — no `SOCK_DGRAM`

`sys_socketpair` accepts only types 1 and 5. `SOCK_DGRAM` (2) over AF_UNIX is
what `/dev/log`, `nscd` and most "fire and forget a message" IPC uses. It needs
real datagram framing, which pipes cannot express.

### G4 — no ancillary data: `SCM_RIGHTS` / `SCM_CREDENTIALS`

Both `sendmsg` paths ignore `msg_control` entirely, and `recvmsg` actively
zeroes `msg_controllen`. So:

- **fd passing is impossible.** This is the mechanism behind systemd socket
  activation, containerd's shim → runc handoff, Wayland buffer passing, and the
  privilege-separation handoff in OpenSSH.
- `SO_PEERCRED` / `SCM_CREDENTIALS` are absent, so nothing can authenticate a
  peer by uid. Any daemon that gates on peer uid must either refuse or trust
  everyone.

### G5 — `SEQPACKET` is a lie, and multi-iovec messages are truncated

The doc comment at `net.rs:145` is honest about the first half:

> NOTE: this approximates SOCK_SEQPACKET with a byte stream — message
> boundaries are not preserved.

Sufficient for libstd's one fixed-size handshake; wrong for anything that sends
two messages. The second half is worse: the **smoltcp** `sys_sendmsg` unix arm
(`net.rs:910`) writes `iovs[0]` only and returns its length, so a caller passing
a header+payload iovec pair silently loses the payload and gets a short count it
has no reason to distrust. `sys_recvmsg` (`net.rs:1061`) has the same
single-iovec limitation. The rump-only `sendmsg` variant coalesces all iovecs
correctly — the two arms of the same syscall disagree.

### G6 — `getsockname` / `getpeername` / `shutdown` / `getsockopt` on unix fds

- `getsockname`/`getpeername` are smoltcp-gated and resolve through
  `get_socket_from_fd`, which only knows the AF_INET table → **EBADF** for a
  `UnixSocket`. A program that calls `getsockname` after `bind` to learn its own
  address (common in test harnesses) fails.
- `sys_shutdown` (`net.rs:620`) returns `0` for any non-AF_INET fd — a
  permissive lie deliberately kept. `SHUT_WR` on a unix socket should send EOF
  to the peer (close the `tx` pipe's write end) and does not.
- `getsockopt(SO_TYPE)` answers `1` (STREAM) unconditionally for a non-socket
  fd, so a `SEQPACKET` or `DGRAM` unix fd misreports its own type.

### G7 — no `S_IFSOCK` anywhere in the stack

`grep -r S_IFSOCK src/ crates/` is empty, and there is **no `mknod`**
(`grep -rn mknod src/syscall/ src/vfs/` is empty). `crates/akuma-ext2/src/ext2.rs:532`
defines only:

```rust
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;
const S_IFLNK: u16 = 0xA000;
```

`S_IFSOCK` (`0xC000`) and the ext2 dirent type byte `EXT2_FT_SOCK` (6) are
missing. There is precedent for a non-regular `st_mode` in the stat path —
`src/syscall/fs.rs:2019` returns `0o10600` (S_IFIFO) for a pipe — so the
plumbing exists; only the socket type is absent. Without it, `stat()` on a bound
socket path cannot report `S_ISSOCK`, which is exactly what every client does
before connecting, and what `rm`/`ls -l` use.

### G8 — no abstract namespace

Linux's `sun_path[0] == '\0'` namespace needs no filesystem at all. It is what
lets a probe (and dbus, and systemd) work on a read-only or missing rootfs, and
it is the *cheapest* thing on this list to implement — a table keyed by bytes,
with no VFS involvement whatsoever. Worth doing first for exactly that reason.

### G9 — flags are ignored

Every `flags` parameter on the unix path is `_flags`. `MSG_PEEK`,
`MSG_TRUNC`, `MSG_WAITALL`, `MSG_DONTWAIT`, `MSG_CMSG_CLOEXEC` and
`MSG_NOSIGNAL` all do nothing. `MSG_DONTWAIT` in particular: a caller that
passes it instead of setting `O_NONBLOCK` gets a **blocking** call, which is a
hang, not a wrong answer.

### G10 — accounting and introspection

`/proc/<pid>/fd/N` renders `socket:[<rx pipe id>]` (`src/vfs/proc.rs:224`),
using a pipe id where Linux puts an inode number, so two endpoints of one pair
report different "inodes" and `lsof`-style matching cannot pair them. There is
no `/proc/net/unix`.

---

## 3. Design

### 3.1 The seam: a host-testable `unix` module in `akuma-net`

The user-facing constraint is "host level tests", and the pure part of AF_UNIX is
large: name binding, the connect/accept rendezvous, datagram queueing, message
framing, credential capture, and the `sockaddr_un` encode/decode. None of it
needs a NIC, a timer, or a page table. So:

**New module `crates/akuma-net/src/unix.rs`, compiled unconditionally — NOT
gated on `smoltcp`.** The rump-only devbox build must keep AF_UNIX (the sysproxy
channel depends on it), and `socket.rs` already establishes the pattern of a
module that stays compiled while its smoltcp internals are gated.

The module owns a `UnixSocketTable` that is a pure state machine over an
injected buffer type. Every decision — "is this connect refused?", "does this
datagram fit?", "which endpoint gets woken?" — is a method on it, testable with
`cargo test -p akuma-net` and no kernel at all. The kernel keeps only what it
must: user-pointer copies, fd allocation, waker registration, and the VFS calls
that create and stat the socket node.

Concretely, the split:

| Lives in `akuma-net/src/unix.rs` (host-tested) | Lives in `src/syscall/net.rs` (kernel) |
|---|---|
| `SockAddrUn` encode/decode, incl. abstract names and the unterminated-`sun_path` edge cases | `copy_from_user` / `copy_to_user` of `sockaddr_un` |
| The bind-name table (path → listener id), collision → `EADDRINUSE` | Creating/unlinking the VFS socket node |
| Listener backlog queue, `accept` pairing, `connect` → `ECONNREFUSED` when unlistened or backlog-full | Blocking/waking (`schedule_blocking`, `pipe_add_poller`) |
| Datagram queue with per-socket byte + message caps, `EAGAIN` / `ENOBUFS` policy | `alloc_net_bounce` bounce buffers |
| SEQPACKET/DGRAM message framing (length-prefixed records) | fd table mutation for `SCM_RIGHTS` |
| Ancillary-data (`cmsg`) parse/serialize, alignment, `CMSG_*` arithmetic | Actually duplicating the passed fds |
| Peer-credential capture at connect time | Reading the current pid/uid/gid |
| Shutdown state (`SHUT_RD`/`WR`/`RDWR`) transitions and what they make readable | Console tracing |

### 3.2 Keeping the buffer, replacing the object

The pipes are fine as the *stream* transport — they already have a 64 KiB
`PIPE_CAPACITY`, refcounts, pollers, EOF/HUP semantics and a waker path that
works under SMP. Rewriting that would re-litigate solved problems. Two changes,
both additive:

1. **Widen the descriptor.** `FileDescriptor::UnixSocket { rx, tx }` becomes
   `UnixSocket { rx, tx, sock: u32 }`, where `sock` indexes the new
   `UnixSocketTable` entry holding the name, type, peer id, credentials,
   shutdown state, option state, and — for `SEQPACKET`/`DGRAM` — the record
   boundaries. `socketpair` allocates an entry with no name and type from the
   caller; every existing pipe-backed fast path keeps working unchanged, because
   `rx`/`tx` still mean what they mean.

   The alternative — a fresh `FileDescriptor::UnixSock(u32)` variant with the
   pipes hidden inside the table — is cleaner but touches all eight call sites in
   §1.3 plus `fd.rs`'s close/clone paths in one commit. Widening keeps each
   phase independently bootable, which matters more here.

2. **Frame `SEQPACKET`/`DGRAM` in the table, not the pipe.** Keep a `VecDeque`
   of record lengths alongside the byte stream. `write` pushes a length; `read`
   pops one and reads exactly that many bytes. The stream case pushes nothing and
   reads as today, so `SOCK_STREAM` costs nothing. This is the whole of G5, and
   it is pure logic — the record-boundary queue is the single most valuable thing
   to host-test, because getting it wrong is a silent truncation.

### 3.3 The name table and the VFS node

Two namespaces, both keyed by `[u8]`:

- **Abstract** (`sun_path[0] == 0`): a plain map from the remaining
  `sun_path[1..sun_len]` bytes to a listener id. No VFS. Do this first (G8) —
  it exercises bind/listen/accept/connect end to end with zero filesystem risk,
  and it is what the probe can use before `S_IFSOCK` exists.
- **Filesystem**: canonicalize the path, then (a) create a real ext2 inode with
  `S_IFSOCK` + dirent type 6 so `stat`/`ls`/`rm` behave, and (b) insert
  path → listener id into the same table. `bind` on an existing path is
  `EADDRINUSE`; `connect` to a path with no live listener is `ECONNREFUSED`
  (the stale-socket-file case every daemon's restart path depends on). Closing
  the listener removes the table entry but — matching Linux — leaves the inode,
  so the daemon must `unlink` it itself.

`S_IFSOCK` needs adding in three places: `crates/akuma-ext2/src/ext2.rs:532`
(the constant + the create path), the dirent type byte, and the `st_mode`
passthrough at `src/syscall/fs.rs:1982` (which already forwards `meta.mode`
verbatim, so it may need nothing).

### 3.4 SCM_RIGHTS

The hard part is not the cmsg arithmetic — it is that a passed fd must survive
in the *sender's* table until the receiver takes it, and must not leak if the
receiver never calls `recvmsg` or dies first. So:

- On `sendmsg` with `SCM_RIGHTS`, clone the refcount for each fd (the same
  `pipe_clone_ref`/`socket_clone_ref` calls `sys_dup` makes at `fs.rs:1470`) and
  attach the `FileDescriptor` values to the queued record in the table.
- On `recvmsg`, allocate fresh fd numbers in the receiver and write the cmsg,
  honouring `MSG_CMSG_CLOEXEC`.
- **On socket teardown, close every fd still attached to an unread record.**
  This is where a leak would live, and it is exactly the class of bug the
  `test_socketpair_close_refcount` self-test was written for. Host-test the
  in-flight accounting (attach → teardown → refcount returns to baseline)
  before writing the kernel half.

Note the ordering hazard: `SCM_RIGHTS` records must stay attached to a *record*,
not to the socket, or a stream socket with two in-flight messages delivers the
second message's fds with the first.

### 3.5 Where the BKL sits

Every unix path must be handled **before** `NetBklGuard::new()` drops the BKL,
exactly as the existing arms are. The comment at `net.rs:426` states the rule:

> AF_UNIX socketpair endpoint: send == write to the tx pipe. Checked BEFORE
> [dropping the BKL] …

The pipe layer's locks were never audited for the BKL-free window, and the
`no-bkl-network` carve-out (`NetBkl`, `net.rs:76`) is justified specifically by
the AF_INET socket table and `NETWORK` carrying their own fine-grained locks.
The unix table would need the same treatment to earn a BKL-free window; until
then, keep it under the BKL and say so in a comment. Getting this wrong
resurfaces as the `[BKL] stuck` class (docs/archive/BKL_VFS_CARVE_OUT.md §8).

---

## 4. Phased plan

Each phase boots and is independently testable. Host tests land **with** the
phase, not after.

### Phase 0 — make the current behaviour honest (no new features)

The smallest change with real value, and it fixes two live defects:

- **Multi-iovec `sendmsg`/`recvmsg` on the unix path.** Coalesce all iovecs in
  the smoltcp arm the way the rump arm already does (`net.rs:976`). Today
  `iovs[0]`-only is a silent payload loss (G5).
- `sys_shutdown` on a `UnixSocket`: `SHUT_WR` closes the `tx` write end so the
  peer sees EOF; `SHUT_RD` makes reads return 0.
- `getsockopt(SO_TYPE)` answers from the recorded type instead of a hardcoded
  `1`.
- `getsockname`/`getpeername` on a `UnixSocket` return a zero-length
  `sockaddr_un` with `sun_family == AF_UNIX` instead of `EBADF` — what Linux
  reports for an unbound socketpair endpoint.
- Honour `MSG_DONTWAIT` (G9) — the one ignored flag whose failure mode is a hang.

Host tests: the iovec coalescing function, and a `sockaddr_un` encoder.

### Phase 1 — the table, `socket(AF_UNIX)`, and the abstract namespace

- `crates/akuma-net/src/unix.rs` with `UnixSocketTable`, the state machine, and
  its host tests. No kernel wiring yet — this phase is *pure* and its whole
  deliverable is `cargo test -p akuma-net` going green on the new module.
- Widen `FileDescriptor::UnixSocket` with `sock: u32`; `socketpair` allocates an
  entry. Everything else keeps working (the field is unread).
- `sys_socket(AF_UNIX, SOCK_STREAM|SOCK_SEQPACKET, 0)` allocates an unbound,
  unconnected entry.
- `bind`/`listen`/`accept`/`accept4`/`connect` for **abstract** names only.
  `connect` creates the two pipes and hands one endpoint to the accepting
  process — the same construction `socketpair` already does, just with the two
  fds landing in different processes.
- Un-gate `BIND`/`LISTEN`/`ACCEPT`/`ACCEPT4`/`CONNECT`/`GETSOCKNAME`/
  `GETPEERNAME`/`RECVMSG` in `src/syscall/mod.rs` so they reach the unix path on
  a rump-only build. The AF_INET arm stays `#[cfg(feature = "smoltcp")]`;
  the dispatch becomes "try unix first, then smoltcp or ENETDOWN".
- epoll: a listener with a non-empty backlog reports `EPOLLIN`. This is the one
  new *readiness* predicate and it belongs next to the existing
  `UnixSocket` arm at `poll.rs:586`.
- Boot self-tests in `src/process_tests.rs` next to the existing five
  (kernel changes need kernel tests).

At the end of Phase 1 a client and a server in the same VM can talk over an
abstract-namespace unix socket, with no filesystem involvement at all. That is
the milestone the probe verifies first.

### Phase 2 — the filesystem namespace and `S_IFSOCK`

- `S_IFSOCK` in `crates/akuma-ext2/src/ext2.rs`, the dirent type byte, and the
  `stat` path.
- `bind` to a pathname: canonicalize, create the socket node, register the name.
- `connect` to a pathname; `ECONNREFUSED` on a stale node with no live listener.
- `unlink` of a socket node.
- Host tests: canonicalization, `EADDRINUSE`, the stale-node decision table.

### Phase 3 — `SOCK_DGRAM` and message framing

- The record-length queue (§3.2) for `SEQPACKET` and `DGRAM`.
- Unconnected `sendto`/`recvfrom` with a destination path — this is what makes
  `/dev/log` work, and `/dev/log` is the single best real-workload smoke test
  because musl's `syslog(3)` is three lines of client code.
- `SO_SNDBUF`/`SO_RCVBUF` accounting so a runaway sender gets `EAGAIN`/`ENOBUFS`
  instead of exhausting the heap.
- Host tests: framing (a 0-byte datagram is a real datagram; a record larger
  than the buffer is `EMSGSIZE` for DGRAM but blocks for STREAM), and the
  buffer-accounting policy.

### Phase 4 — ancillary data

- `SCM_RIGHTS` per §3.4, with the in-flight teardown accounting host-tested
  first.
- `SO_PEERCRED` and `SCM_CREDENTIALS` from credentials captured at connect.
- `MSG_CMSG_CLOEXEC`, `MSG_PEEK`, `MSG_TRUNC`.

### Phase 5 — introspection and size

- `/proc/net/unix`; real inode numbers in `/proc/<pid>/fd` so both endpoints of a
  pair agree (G10).
- An `sc-unix-socket` feature so the `extreme-size` 4 MB profile can compile out
  Phases 2-5 while keeping `socketpair` (which `rustc` and the rump sysproxy
  need). Follow the existing `sc-*` pattern at `Cargo.toml:489-497`, including
  the no-op `ExecRuntime` stub the Tier-2 entries document.

---

## 5. Host tests

`crates/akuma-net/src/unix_tests.rs`, registered from `lib.rs` under `#[cfg(test)]`
next to the existing `tests` module (there was a `lock_tests` too until it was
deleted 2026-08-30), run by the documented command:

```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

The point of putting the state machine in `akuma-net` is that these tests need
no VM, so they run in a second and gate the pre-commit hook. What to cover, in
descending order of "would silently corrupt data if wrong":

**Message framing (Phase 3, but write the tests in Phase 1).** A record queue is
where truncation bugs hide, and truncation is invisible: the caller gets a
plausible short count.
- STREAM: two 10-byte writes then one 20-byte read returns 20 (coalesced).
- SEQPACKET: two 10-byte writes then one 20-byte read returns **10** — boundary
  preserved. This is the assertion G5 currently fails.
- SEQPACKET: a 10-byte record read into a 4-byte buffer returns 4 and **discards
  the remaining 6** with `MSG_TRUNC` set — not "leaves them for the next read".
- DGRAM: a 0-length datagram is deliverable and distinguishable from EOF.
- DGRAM: a record larger than `SO_SNDBUF` is `EMSGSIZE`, not a partial write.

**`sockaddr_un` codec.** The struct is 110 bytes of `sun_path` and every field is
a trap.
- Unterminated `sun_path` (a client that fills all 108 bytes) must not read past.
- `sun_len` shorter than the string; `sun_len == 2` (unbound); `sun_len` longer
  than the buffer.
- Abstract names: leading NUL, embedded NULs preserved verbatim, and the
  length taken from `sun_len` rather than from `strlen`.
- Round-trip: encode(decode(x)) == x for all of the above.

**Connect/accept rendezvous.**
- `connect` to an unbound name → `ECONNREFUSED`.
- `connect` to a bound-but-not-listening name → `ECONNREFUSED`.
- Backlog full → `EAGAIN` for a non-blocking connect, queued for a blocking one.
- `accept` on an empty backlog with no waiter → `EAGAIN`.
- Listener closed with N queued connects → all N peers see EOF, and the
  accounting returns to baseline (no leaked entries). Run this as an explicit
  leak assertion, in the spirit of `test_socketpair_close_refcount`.

**Name table.**
- `bind` twice on one name → `EADDRINUSE`.
- `bind` on a name whose listener has closed → succeeds.
- Table entry count returns to zero after every socket is dropped.

**Shutdown matrix.** For each of `SHUT_RD`/`SHUT_WR`/`SHUT_RDWR` × each socket
type, assert what becomes readable, what becomes writable, what returns 0, and
what returns `EPIPE`. This is a table test, and it is the readiness contract
epoll is derived from — the delayed-first-byte investigation
(`docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`) turned on exactly this kind of
predicate being wrong for AF_INET.

**In-flight `SCM_RIGHTS` accounting (Phase 4).** Attach two fds to a queued
record, tear the socket down unread, assert both refcounts return to baseline.
Then the same with the record read normally. The failure mode is a silent fd
leak, which no probe will notice.

**Credentials.** Captured at connect time, not at send time — a sender that
changes uid after connecting still reports the connect-time uid.

---

## 6. The probe: `nettest-unix`

`userspace/nettest` already holds four probes for three investigations, built
by two scripts, and its README's table is the index. Add a fifth row rather
than a new directory — the reason is the pattern `nettest-connect` established
and documented under **"The Linux control arm"**: a static
`aarch64-unknown-linux-musl` binary runs unchanged under Docker Linux, so every
verdict has a reference answer. For AF_UNIX that is worth more than for TCP,
because there is no external server to disagree with — a unix-socket probe is
entirely self-contained, so *the only* way to know a verdict is a kernel bug and
not a probe bug is to run the same binary on Linux.

```
userspace/nettest/rust/unixsock/     -> bootstrap/bin/nettest-unix
```

- Standalone crate with an empty `[workspace]` table (same opt-out as
  `connect/` and `stdlib/`), `libc = "0.2"` as the **only** dependency, raw
  syscalls throughout. No `std::os::unix::net` — that wrapper would hide which
  syscall answered what, which is the whole output of the probe.
- Add a `unix)` case to `build-musl.sh` and to its `all)` line and usage string;
  it needs no new toolchain setup.
- Add the row to `userspace/nettest/README.md`'s table and a `Part 4` section.

### Modes

Each mode prints the same `[probe]` line vocabulary the sibling probes use, and
ends with one `RESULT <mode> verdict=… ` line so a run diffs directly against
the Linux arm.

| Mode | What it exercises | Passes at phase |
|---|---|---|
| `pair [stream\|seqpacket]` | `socketpair` + read/write/sendmsg/recvmsg both ways; **two messages** to catch G5 | 0 |
| `iovec` | `sendmsg` with 3 iovecs, assert the full concatenation arrives | 0 (currently FAILS: payload lost) |
| `shutdown` | the full `SHUT_*` matrix, one line per cell | 0 |
| `abstract` | fork; child binds `\0akuma-probe`, listens, accepts, echoes; parent connects and round-trips | 1 |
| `path <p>` | same over a filesystem path; then `stat(p)` and assert `S_ISSOCK` | 2 |
| `stale <p>` | bind, exit without unlinking, re-bind → must succeed; connect to the stale node → `ECONNREFUSED` | 2 |
| `dgram <p>` | unconnected `sendto`/`recvfrom`, incl. a 0-length datagram | 3 |
| `syslog` | `connect("/dev/log", SOCK_DGRAM)` and send one RFC3164 line — the real-workload smoke test | 3 |
| `passfd` | `SCM_RIGHTS`: pass a `memfd`/tmpfile fd across a pair, read through it, and assert the sender can close its copy first | 4 |
| `peercred` | `SO_PEERCRED` reports the peer's real pid/uid | 4 |
| `poll [--wait poll0\|poll\|select\|epoll]` | readiness on an unread pair, a full pair, a listener with a pending connect, and after each shutdown — across all four readiness syscalls | 1+ |
| `stress <n>` | n sequential connect/accept/echo/close cycles; histogram + a leak check on `/proc/<pid>/fd` count | 1+ |

`poll`'s four `--wait` modes are copied deliberately from `nettest-connect`:
`poll0`/`poll` → `sys_ppoll`, `select` → `sys_pselect6`, `epoll` →
`sys_epoll_pwait`. That bisect is what found the `_exceptfds_ptr` bug
(README Part 3, "Outcome"), and the unix path will have its own version of the
same class — a listener's `EPOLLIN` is a **new** readiness predicate, and it must
report identically through all four.

`stress` exists because the leak classes in this design (unread `SCM_RIGHTS`
records, closed listeners with queued connects, name-table entries) are all
*accumulating* failures that a single round trip cannot see.

### Reading it

| verdict | meaning |
|---|---|
| `OK` | every assertion in the mode held |
| `UNSUPPORTED` | a syscall returned `EAFNOSUPPORT`/`ENOSYS` — the feature is not built yet, not broken |
| `TRUNCATED` | data arrived short or a message boundary was lost — G5 class |
| `LEAK n` | fd count or table count did not return to baseline after n cycles |
| `READINESS <syscall>` | one of the four readiness syscalls disagreed with the others |
| `FAIL <syscall>=<errno>` | a syscall failed where Linux succeeds |

A verdict that differs between the Linux arm and the guest is a kernel
divergence; one that matches on both is a probe bug. Run the Linux arm first —
that is the control, and it is free.

### Wiring

```bash
userspace/nettest/rust/build-musl.sh unix   # -> bootstrap/bin/nettest-unix
scripts/populate_disk.sh                    # -> /bin/nettest-unix
```

Then, in the guest, the phase-appropriate ladder:

```
nettest-unix pair stream
nettest-unix pair seqpacket
nettest-unix iovec
nettest-unix shutdown
nettest-unix abstract
nettest-unix path /tmp/probe.sock
nettest-unix stale /tmp/probe.sock
nettest-unix dgram /tmp/dgram.sock
nettest-unix syslog
nettest-unix passfd
nettest-unix peercred
nettest-unix poll --wait epoll
nettest-unix stress 200
```

---

## 7. Verify

Per phase, in this order — the cheap gate first:

1. **Host unit tests.**
   ```bash
   cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
   ```
   The new `unix_tests` module must be green. Nothing else proceeds until it is.

2. **Linux control arm.** Build the probe for the host triple and run the full
   ladder under Docker Linux. Every mode for the implemented phases must print
   `OK`. A mode that fails here is a probe bug — fix it before touching the
   kernel, or the guest run means nothing.

3. **Boot self-tests.** `cargo run --release` and confirm the new
   `[Test] unix_*` lines print `PASSED`, alongside the five existing
   `socketpair` tests. Count with `grep -ac PASSED` (the boot-count methodology
   note: the docs' figures are that count minus 100).

4. **Guest probe ladder.** Boot, SSH in on 2222, run the ladder from §6, and
   diff the `RESULT` lines against the Linux arm captured in step 2.

5. **Both build targets.** AF_UNIX is not smoltcp-gated, so it must be proven on
   the rump-only devbox too — that build is the one where the un-gating in
   `src/syscall/mod.rs` is load-bearing:
   ```bash
   scripts/build_devbox_smoltcp.sh && overlays/devbox/run-smoltcp.sh
   scripts/build_devbox.sh && overlays/devbox/run.sh     # RUMP_NIC=1
   ```
   On the rump build, the acceptance check is that **box 0's rump stack still
   comes up** — the fd-3 sysproxy channel is a `UnixSocket`, and any regression
   in the descriptor layout or the `sendmsg` coalescing kills the handshake
   silently (§1.2).

6. **`extreme-size` floor.** `scripts/build_extreme_size.sh` must stay under the
   4.0 MB `IMAGE_SIZE` guardrail. Until Phase 5's `sc-unix-socket` feature
   exists, check the delta each phase adds; if it eats the floor, that feature
   moves earlier in the plan.

7. **SMP.** Run the boot suite at `SMP=1` and `SMP=4`. The unix table is new
   shared mutable state reachable from every core, and the BKL question in §3.5
   is unresolved by design — a `[BKL] stuck` line or a hang at `SMP=4` means the
   table needs the `PreemptGuard` treatment `NETWORK`/`SOCKET_TABLE` get.

---

## 8. Risks

- **The rump sysproxy is the blast radius.** It is the only in-tree consumer of
  the current implementation that a regression can kill silently — the symptom
  is "box 0's rump stack never comes up", several layers away from the change.
  Test the rump devbox every phase, not at the end.
- **`rustc` links through `socketpair`.** A regression in the pair path breaks
  self-hosting, and it surfaces as a linker error, not a socket error.
- **The BKL window (§3.5).** The existing arms are all deliberately handled
  *before* the BKL drop. New code that lands after `NetBklGuard::new()` will
  work at `SMP=1` and produce the `[BKL] stuck` class under load.
- **`SCM_RIGHTS` fd lifetime.** The one design in this document where the
  failure mode is a leak no probe reports. Host-test the accounting before
  writing the kernel half; that ordering is the mitigation.
- **Size.** Phases 2-5 add a table, a name map, a record queue, and cmsg
  handling to an image with a 4.0 MB floor. The `sc-unix-socket` gate is not
  optional; only its position in the plan is negotiable.

---

## Background

- `docs/archive/RUMP_SYSPROXY_LATENCY_FIX.md` §3q — why the rump-only
  `sendmsg` coalesces iovecs, and why one write must equal one wake.
- `docs/archive/BKL_VFS_CARVE_OUT.md` §8 — the dropped-window ledger and the
  `[BKL] stuck` conversion that §3.5 is guarding against.
- `docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md` — the four AF_INET readiness
  defects; the shutdown/readiness matrix in §5 is written against that lesson.
- `docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md` and
  `docs/runbooks/cargo-cannot-reach-crates-io.md` §3 — the `_exceptfds_ptr` bug
  the four-way `--wait` bisect found, which is why §6's `poll` mode has four.
- `userspace/nettest/README.md` — the probe index §6 adds a fifth row to, and
  the source of the Linux control-arm method (§ "The Linux control arm" under
  Part 3). Note it cites `docs/archive/LINUX_AB_PROBE.md`, which does **not**
  exist in the tree — a dangling reference it shares with
  `docs/runbooks/cargo-cannot-reach-crates-io.md:286` and
  `rust/connect/src/main.rs:99`. The method is described in the README section
  itself; do not go looking for that file.
- `docs/archive/RUST_TOOLCHAIN.md` §4d — why `socketpair` exists at all.
