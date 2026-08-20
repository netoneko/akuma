# Getting nginx running on Akuma — what it actually cost

**Status: FIXED. nginx serves real requests end-to-end** — `curl` gets a clean
`HTTP/1.1 200 OK` from both inside the guest (loopback) and from the host via
the forwarded port. 2026-08-20, `devbox-smoltcp`, apk `nginx-1.30.4-r1`.

Goal: run the official `nginx` package (not the Docker image) on the barest
config possible and benchmark it against nginx-in-Docker. The opening question
was "it can't drop privileges because there are no users in this system — how
much would it cost to add the user syscalls, or stub them?" The honest answer
turned out to be **zero syscalls** — see Issue A. Three more issues sat behind
it before a client could even complete a handshake.

## The config used

Deliberately the smallest thing that parses, serving a hardcoded body (no
filesystem I/O in the request path):

```
user root;
worker_processes 1;
pid /tmp/nginx.pid;
error_log /tmp/nginx-error.log;
events {}
http {
    server {
        listen 8080;
        location / { return 200 "hello from akuma\n"; }
    }
}
```

`pid`/`error_log` point at `/tmp` because `/run/nginx` doesn't exist on this
rootfs and apk's postinstall didn't create `/var/log/nginx` reliably either —
not a kernel gap, just a stock-Alpine-package assumption about directories
that get created elsewhere in a real Alpine boot.

## Issue A: "no users in this system" is a missing file, not a missing syscall

### Symptom

```
nginx: [emerg] getpwnam("root") failed (2: No such file or directory) in /etc/nginx/nginx.conf:1
```

Note it fails to resolve **`root`** — not some unprivileged `nginx` user.

### Cause

`/etc/passwd` and `/etc/group` do not exist anywhere on the rootfs:

```
# ls -la /etc/passwd /etc/group
ls: /etc/passwd: No such file or directory
ls: /etc/group: No such file or directory
```

`getpwnam()` is pure musl libc — it parses `/etc/passwd` itself, no syscall,
no NSS. There is nothing for the kernel to implement here: this was never a
"missing user syscall" problem. `setresuid`/`getresuid`/`getgroups` etc. are
already implemented as accepting no-ops (`DEVBOX_ISSUES.md` Issue 15,
`../runbooks/run-redis.md` §"Why `--entrypoint`") — the credential-syscall
machinery nginx needs was already there. It just had nothing to read the
username from.

### Fix

```
root:x:0:0:root:/root:/bin/sh
```
```
root:x:0:
```
written to `/etc/passwd` and `/etc/group`. Zero kernel changes. Since the
config says `user root;`, every `setuid`/`setgid` nginx subsequently issues is
a same-uid no-op — real privilege-dropping (a distinct `nginx` user) was never
exercised and remains open per Issue 15.

**Cost of the original question, answered:** the "add user syscalls or stub
them" work was already done before this session started. What was missing was
two lines of static file content.

## Issue B: master never calls `fork()` for the worker

### Symptom

```
nginx: [alert] ioctl(FIOASYNC) failed while spawning "worker process" (25: Not a tty)
```
then, once B1 alone was fixed:
```
nginx: [alert] fcntl(F_SETOWN) failed while spawning "worker process" (22: Invalid argument)
```
Only ONE `nginx` process ever exists; `ps` never shows a worker; `[PSTATS]`
for the master shows no `clone` syscall at all.

### Cause

nginx's `ngx_spawn_process` (`os/unix/ngx_process.c`) sets up the
master↔worker channel *before* forking:

```c
if (ioctl(channel[0], FIOASYNC, &on) == -1) { ...alert...; return NGX_INVALID_PID; }
if (fcntl(channel[0], F_SETOWN, ngx_pid) == -1) { ...alert...; return NGX_INVALID_PID; }
...
pid = fork();
```

Both enable SIGIO-on-data-ready for the channel socketpair fd — a nicety
nginx uses so a worker gets nudged even outside its own event loop. Neither
was implemented in Akuma:

- `ioctl(FIOASYNC)` (`0x5452`) wasn't in `sys_ioctl`'s handled-command list
  (`src/syscall/term.rs`), so it fell through to the generic `fd > 2 → ENOTTY`
  path.
- `fcntl(F_SETOWN)` (`8`) wasn't in `sys_fcntl`'s match (`src/syscall/fs.rs`),
  so it fell to the `_ => EINVAL` catch-all.

nginx treats **either** failure as fatal and returns before ever calling
`fork()`. This is a general trap, not nginx-specific: anything that tries to
arm SIGIO on a pipe/socket before forking hits the same wall.

### Fix

Akuma delivers no SIGIO for any fd — there is no real behavior to implement.
Both are now accepted no-ops, the same pattern already used for
`setresuid`/`getresuid`/etc:

- `src/syscall/term.rs`: `FIOASYNC => { return 0; }` alongside `FIOCLEX`/`FIONCLEX`.
- `src/syscall/fs.rs`: `F_SETOWN | F_GETOWN => 0` alongside the advisory-lock no-ops.

~10 lines total. After this, the master calls `fork()`, `ps` shows two `nginx`
processes, and the worker survives its own startup — for about 20ms.

## Issue C: the worker crashes ~20ms after forking

### Symptom

```
[Fault] Data abort from EL0 at FAR=0x8, ELR=0x100514fc, ISS=0x7
[Fault] Process <pid> (/usr/sbin/nginx) SIGSEGV after 0.02s
```

Disassembly (`llvm-objdump` against the pulled-off-disk nginx binary,
matching a `0x10000000` PIE load base against the register dump) puts the
fault inside `ngx_epoll_process_events`: `wev = c->write; ...wev->active...`
— `c->write` is NULL.

### Cause

With `SYSCALL_DEBUG_INFO_ENABLED` + `SYSCALL_DEBUG_EPOLL_EDGE` on
(`src/config.rs`), the trace shows exactly two `socketpair()` calls all
session. The worker creates one for itself right after forking
(`ngx_event_process_init`'s notify/thread-pool setup), registers one end
(`fd=11`) with `epoll_ctl ADD ... events=0x80002001` (`EPOLLIN|EPOLLRDHUP`,
no `EPOLLOUT`), gets one clean `EPOLLIN` delivery — and then, with no further
syscall in between, both pipe objects behind the pair get destroyed
(`[pipe] DESTROY id=3/4 (both counts 0)`), meaning nginx closed **both** ends
almost immediately (`sys_close` has no debug print, hence the apparent gap).

Real Linux implicitly drops a fd from every `epoll` instance's interest list
the moment the fd is `close()`'d (`eventpoll_release_file` walks
back-references from the file to its `epitem`s). **Akuma's `close()` does
not do the equivalent** — `sys_close` (`src/syscall/fs.rs`) tears down the
pipe/socket resource but never touches `EPOLL_TABLE`. So the next
`epoll_wait` still finds the stale interest-list entry for `fd=11`,
`epoll_check_fd_readiness` calls `proc.get_fd(11)` → `None`, and its
"fd not found" fallback (`src/syscall/poll.rs`) synthesizes
`EPOLLHUP|EPOLLERR` — a *real* event delivered to userspace for a fd the
caller already closed, which real Linux can never produce.

nginx's own crash-recovery logic in `ngx_epoll_process_events` treats that as
license to force both handlers to run (`if (revents & (EPOLLERR|EPOLLHUP)) {
revents |= EPOLLIN|EPOLLOUT; }`) — and dereferences the `write` half of a
connection object it had already torn down along with the fd.

### Fix

`sys_epoll_wait`'s per-fd loop (`src/syscall/poll.rs`) now checks whether the
fd is still open in the current process before calling
`epoll_check_fd_readiness`; if not, it prunes the stale `interest_list` entry
and skips it — no synthetic event — instead of falling into the generic
"fd not found" fallback. A ~15-line, narrowly-scoped fix (touches only the
`epoll_wait` loop, not the shared readiness-check fallback that `ppoll`/
`select` also use, to avoid changing their semantics).

After this fix, both `nginx` processes stay alive indefinitely and the
listener answers on `:8080` — but only when nothing tries to connect to it.

## Issue D — FIXED: a second `listen()` call orphans the first call's backlog

### Symptom

With Issues A–C fixed, `nginx` (master + worker, both alive, no crash) never
answers a client:

```
$ curl -m 5 http://127.0.0.1:8080/
* Connected to 127.0.0.1 (127.0.0.1) port 8080
> GET / HTTP/1.1
...
* Operation timed out after 5003 milliseconds with 0 bytes received
```

`/proc/net/tcp` shows the listener in `LISTEN` and nothing else — the client's
`connect()` returns (curl prints "Connected"), but the connection never
reaches `ESTABLISHED` at the smoltcp layer. All 32 pre-allocated backlog
handles (`MAX_BACKLOG`, `crates/akuma-net/src/socket.rs`) sit in `Listen`
state forever — confirmed by instrumenting `socket_can_recv_tcp`'s
`Listener` branch (`src/syscall/net.rs`) to print every handle's state on
each check; none ever moves.

### Isolation — and a wrong turn worth recording

- A single, non-forking process (`busybox nc -l -p <port>`, or a second `nc`
  as the client) on a fresh boot: **works**. Data flows both directions.
- `nginx` with `master_process off;` (no `fork()` at all): **works**.
- `nginx` in its normal shape (master `listen()`s, then `fork()`s a worker
  that inherits the fd and runs the event loop): **hung**, reproducibly.

That pattern — works without fork, fails with fork — pointed straight at
"a listening socket can't survive `fork()`". It was the wrong turn. Two
minimal, purpose-built C reproducers (cross-compiled with the host's
`aarch64-linux-musl-gcc`, pushed to the VM), replicating fork+listen+accept
in exactly nginx's shape — plain blocking `accept()` in the child, then a
second version using non-blocking `accept()` driven by `epoll_wait` with
edge-triggered eventfds registered ahead of the listener, `EPOLLRDHUP`,
even `curl` specifically as the client instead of `nc` — **all worked**.
Fork-plus-listening-socket is fine in general; something exclusive to real
nginx was still missing. `box_id` isolation, the worker not driving
`smoltcp_net::poll()`, and the wake-on-state-change path were all checked
directly and ruled out along the way (each fires as expected).

### Cause

Root-caused by adding one targeted print to `sys_listen` (`src/syscall/
net.rs`) and reading the ordering directly, rather than continuing to guess
at reproducers:

```
[syscall] listen(fd=6, backlog=511) idx=2 pid=10
[syscall] listen(fd=6, backlog=511) idx=2 pid=10
[syscall] clone: forking PID 10 -> 11 (flags=0x11)
```

nginx's master calls `listen()` on the same fd **twice**, both before the
fork. `socket_listen` (`crates/akuma-net/src/socket.rs`) was not idempotent:
every call unconditionally `.take()`s the table slot and builds a brand new
`Listener` with `MAX_BACKLOG` (32) fresh smoltcp sockets. For the `Stream`
variant it closes the old handle first; for `Listener` it did **not** —
the old `handles: VecDeque<Handle>` was simply dropped, in Rust terms, with
no call to `smoltcp_net::socket_close` on any of them.

Those 32 orphaned sockets don't go away: they're still live inside smoltcp's
`SocketSet`, still bound and `Listen`-ing on port 8080 — just no longer
referenced by `table[idx]`. The *second* `listen()` call's 32 fresh handles
are what `table[idx].handles` now points to, and they're what
`socket_can_recv_tcp`/`has_pending_connection`/`socket_accept` all walk. Two
independent sets of 32 `Listen`-state sockets end up bound to the same port;
smoltcp has no way to know one set is orphaned and will happily complete a
handshake on either. Every SYN that landed on the orphaned first set went
`Established` invisibly and sat there forever — `accept()` never saw it,
because nothing was still looking at that set. That this reproduced 100% of
the time (not intermittently) says smoltcp's internal matching order is
stable — first-created-first-matched, so the orphaned set with the earlier
allocation order won every single handshake.

Why nginx calls `listen()` twice wasn't chased further — it reproduced
identically across every run in this session, on an unmodified upstream
Alpine `nginx` binary, so it's nginx's real behavior on this platform, not a
test artifact. The kernel bug is that a second `listen()` shouldn't have been
able to leak the first call's backlog regardless of why it happened; real
Linux permits calling `listen()` again on a live socket purely to adjust the
backlog depth, without disturbing the existing queue.

### Fix

`socket_listen` (`crates/akuma-net/src/socket.rs`) now closes **every**
handle in an existing `Listener`'s `handles` list, the same way it already
closed a single `Stream` handle, before replacing the table entry:

```rust
match sock.inner {
    SocketType::Stream(h) => smoltcp_net::socket_close(h),
    SocketType::Listener { handles, .. } => {
        for h in handles { smoltcp_net::socket_close(h); }
    }
    _ => {}
}
```

~10 lines. Verified end-to-end: `curl` gets a clean `HTTP/1.1 200 OK` /
`hello from akuma`, repeatably, from both guest loopback and the host via the
forwarded port.

A minimal repro for anyone who wants to chase *why* nginx calls `listen()`
twice (cosmetic curiosity now, not a blocker): any program that calls
`listen()` twice on the same fd before `accept()`-ing anything will leak a
backlog the same way, fork or no fork.

## Net effect on the original question

"How much would user syscalls cost, or stubbing them?" — the real
credential/user-syscall surface (Issue A) cost nothing; it was already done.
Getting nginx from "won't even start" to "serves real requests end-to-end"
cost four small, narrowly-scoped kernel fixes, ~45 lines total: two accepting
no-ops (Issue B), one `epoll_wait` bookkeeping fix (Issue C), and one
`listen()` idempotency fix (Issue D). None of the four is a missing-syscall
problem in the sense the opening question assumed — they're all either
"accept and ignore, we don't need the real behavior" or "don't leak/desync
kernel state across a call real programs make more than once."

## Background

- [`../runbooks/run-docker-image.md`](../runbooks/run-docker-image.md) §Troubleshooting
  — the `setpriv` re-exec-forever trap or `--entrypoint`; not what blocked
  this (native `apk` install, not a `box run` container).
- [`../runbooks/run-redis.md`](../runbooks/run-redis.md) §"Why `--entrypoint`"
  — the credential-syscall no-op pattern Issue B follows.
- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 15 — no per-process
  credentials; still the reason real privilege-dropping (a distinct `nginx`
  user, not `root`) remains unexercised.
