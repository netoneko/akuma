# Getting nginx running on Akuma — what it actually cost

**Status: nginx starts and serves real requests end-to-end (Issues A-D
FIXED)** — `curl` gets a clean `HTTP/1.1 200 OK` from both inside the guest
(loopback) and from the host via the forwarded port. Benchmarked against
Docker nginx over the NIC: `connect` is a dead heat, `echo` has a healthy
median but a 12-24x tail, `http` mode and any sustained connection churn hit
a still-open degradation (**Issue E**) that a second worker does not fix.
2026-08-20, `devbox-smoltcp`, apk `nginx-1.30.4-r1`.

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

## Benchmark: nginx vs Docker nginx, via the NIC

Both arms run the *exact same* bare-bones config above (Docker's mounted
read-only over the stock image's `/etc/nginx/nginx.conf`, `user nginx;`
instead of `user root;` since Docker has real users). Client is the macOS
host, both servers reached through a forwarded port —
`scripts/benchmarks/bench_nic_rtt.py`, `SMP=4 MEMORY=4096` vs `docker run
--cpuset-cpus=0-3 -m 4g`. Because both servers are the same program for once
(unlike `userspace/httpd` vs nginx), all three of the script's modes are
legitimately comparable here, not just `connect`.

### `connect` — raw TCP handshake, isolates the stack

500 samples, 0 errors both arms:

| | Akuma | Docker |
|---|---|---|
| min | 108.4 us | 111.2 us |
| p50 | 130.5 us | 132.3 us |
| p90 | 173.0 us | 152.3 us |
| p99 | 217.1 us | 254.1 us |
| rate | 6567/s | 6567/s |

Essentially a dead heat — a real change from this repo's historical baseline
(`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`: ~50 us akuma-fwd vs ~17 us docker-fwd,
measured against redis). The loopback/networking work on this branch
(`move loopback to noalloc path`, `more loopback interface gains`) shows up
directly here.

### `echo` — one request/response on an already-open keep-alive connection

500 samples, 0 errors both arms (`--payload 'GET / HTTP/1.1\r\nHost: x\r\n
Connection: keep-alive\r\n\r\n' --expect 'hello from akuma'`):

| | Akuma | Docker |
|---|---|---|
| min | 79.7 us | 101.3 us |
| p50 | 253.7 us | 140.3 us |
| p90 | **2754.4 us** | 167.3 us |
| p99 | **5352.6 us** | 221.6 us |
| rate | 1469/s | 6972/s |

Median is ~1.8x Docker's; the tail is 12-24x. Tight median + huge tail is the
signature the benchmark script's own docstring calls out: some round trips
are waiting for a scheduler tick (the `blocking_relax` WFI park) rather than
for the wire. `min` being *lower* than Docker's says the fast path is
genuinely fast — it's specifically the tail that costs.

### `http` mode — never got clean numbers; see Issue E below

Fresh connect + GET + read-until-close per sample. This is where connection
churn (Issue E) showed up directly: it triggered the exact degradation this
mode's rapid-fire pattern is built to.

## Issue E — OPEN: nginx degrades under connection churn; 2 workers make it worse, not better

**Status: OPEN, root cause not fixed.** Found while benchmarking (above), not
while getting nginx to start. Distinct from Issues A-D: this is not "nginx
can't start" but "nginx stops answering after enough traffic," and — as E2
below shows — adding a second worker doesn't help; it adds a *new* failure
mode on top.

### E1: a `--mode connect` run leaves the worker permanently unable to answer

500 handshakes with `SO_LINGER 0` (the benchmark script's own choice, so
teardown is a single RST rather than a FIN exchange — see the script's
docstring) against a single-worker nginx: the run itself completes cleanly
(0 errors), but the *very next* real request afterward gets `Connection
reset by peer` — consistently, indefinitely (30+ s of retries, no recovery).
`ps`/`PSTATS` show the worker still alive, not crashed, not looping. The
error log has one new line each time this state is entered:

```
write() to "/var/lib/nginx/logs/access.log" failed (20: Not a directory) while logging request
```

(`/var/lib/nginx/logs` is a file, not a directory, on this rootfs — a stock
Alpine packaging gap, not a kernel bug; harmless on its own since nginx logs
the alert and keeps serving — but it's the only new signal that lines up with
when things stop working, so it's recorded here even though it wasn't
confirmed as causal.)

**Killing and restarting nginx fixes it instantly** — same kernel, same
listening socket, same port, brand new worker, first request succeeds. So
this is *worker-process* state degrading under heavy connect+immediate-RST
load, not a kernel-level socket/fd leak that would survive the process exit.
The natural read: nginx's own free-connection pool isn't being returned to
correctly when a connection is accepted and then RST'd before any data
exchange — 500 of those in a row exhausts something nginx tracks internally
(`worker_connections`, default 512, is suspiciously close to 500). Not
confirmed; whether the RST'd connections are being cleaned up on Akuma's side
in a way nginx's accept/close bookkeeping doesn't expect is the open question.

### E2: `worker_processes 2` doesn't recover from E1 — and adds its own bug

Tried, at the point this doc was written, specifically to see if a second
worker would let the listener keep serving while one worker degraded. It
does not:

- The same `--mode connect` churn test, run against 2 workers, hits E1
  identically — the next real request gets `Connection reset by peer`,
  every time, from every one of several follow-up attempts.
- The churn test's own tail latency was *worse* with 2 workers, not better:
  `p99` 2725 us vs 217 us single-worker, `max` 30816 us vs 398 us — some
  extra contention or scheduling cost from the second worker, not a win.
- A **new, distinct error appears only with 2 workers**, never with 1:
  ```
  recvmsg() returned invalid ancillary data level 0 or type 0
  accept4() failed (103: Connection aborted)
  ```
  The first is nginx's inter-worker channel (`ngx_channel.c`) reading a
  `recvmsg()` control message it doesn't recognize. With one worker there is
  nothing to pass messages *to*, so this path is simply never exercised —
  it's new precisely because `worker_processes 2` is new. Root cause traced
  to `sys_sendmsg`'s `AF_UNIX` fast path (`src/syscall/net.rs`,
  `fd_is_unix_socket(fd)` branch): it reads `msg_control`/`msg_controllen`
  off the caller's `struct msghdr` but never forwards them — the whole branch
  is `return super::fs::sys_write(fd, iov.iov_base, iov.iov_len)`, a plain
  byte-stream write with **no ancillary-data support at all**. `sys_recvmsg`'s
  matching branch does correctly zero `msg_controllen` back on the way out,
  so this isn't as simple as "the field is garbage" — the exact mechanism by
  which nginx ends up reading a non-empty, zeroed cmsg header wasn't chased
  further, but the underlying gap is real and verified by inspection: **Akuma
  has no `SCM_RIGHTS` (or any other ancillary data) support in `sendmsg`/
  `recvmsg` over `AF_UNIX` sockets**, full stop. Any program that passes a fd
  or credentials over a Unix socket — nginx's multi-worker channel is one,
  but it's a standard enough pattern that others will hit it too — will find
  the control data silently vanishes.
- `accept4() failed (103: Connection aborted)` from the *other* worker in the
  same run — matches a connection that reached the listener's backlog and
  then died (RST or abort) before `accept()` could claim it, consistent with
  the same churn pattern as E1, just observed from the kernel-error side
  instead of the client side.

**Net answer to "can 2 workers help it recover": no.** Given the same load
that breaks a single worker, two workers break the same way *and* surface an
unrelated, real ancillary-data gap that would block anything relying on
`SCM_RIGHTS` over a Unix socket, independent of nginx or of E1.

### Why `http` mode couldn't get clean numbers

Separately from E1/E2: even on a freshly restarted, otherwise idle
single-worker nginx, `--mode http` (fresh connect, send a minimal HTTP/1.0
request, read until the peer closes) reliably times out — even with 100 ms
gaps between samples, so it isn't rate/churn-related the way E1 is. A raw
one-shot probe confirms the response body **is** delivered (`HTTP/1.1 200
OK` and the 17-byte body arrive over a single `recv()`), so the connection
just isn't closing promptly enough afterward for the client's
read-until-EOF loop to see it inside the 5 s timeout. Whether this is nginx
not calling `close()` in the code path this minimal a request takes, or
Akuma's TCP stack delivering the FIN late for this specific short-lived-
connection shape, is unresolved — flagging it here rather than chasing a
fifth rabbit hole in the same session.

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

What's left (Issue E) isn't a missing syscall either: it's real load-bearing
behavior — connection lifecycle bookkeeping under churn, and `SCM_RIGHTS`
ancillary data over `AF_UNIX` sockets — that nothing in A-D needed and that
only showed up once the benchmark started exercising nginx harder than "does
it start and answer one request." `connect` and `echo` say the fast path is
genuinely competitive with Docker; E is what stands between that and a
server that survives real traffic.

## Background

- [`../runbooks/run-docker-image.md`](../runbooks/run-docker-image.md) §Troubleshooting
  — the `setpriv` re-exec-forever trap or `--entrypoint`; not what blocked
  this (native `apk` install, not a `box run` container).
- [`../runbooks/run-redis.md`](../runbooks/run-redis.md) §"Why `--entrypoint`"
  — the credential-syscall no-op pattern Issue B follows.
- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 15 — no per-process
  credentials; still the reason real privilege-dropping (a distinct `nginx`
  user, not `root`) remains unexercised.
