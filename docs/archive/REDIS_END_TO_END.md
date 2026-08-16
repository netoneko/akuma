# Redis end to end: from "connection refused" to the official image

**Date:** 2026-08-16
**Status:** working, with one named gap (§6).
**Short version:** `redis-server` had been *starting* since the
[`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) fixes, but no client on the
same machine could reach it. Two bugs in `crates/akuma-net/src/socket.rs`, both
reported as `ECONNREFUSED`. With those fixed, the official `redis:alpine` image
pulled from Docker Hub runs in a box and serves clients on the host.

How to actually run it: [`../runbooks/run-redis.md`](../runbooks/run-redis.md).

---

## 1. How long it took

The user's guess was "an hour or something". Close, for the headline result.
Wall clock on 2026-08-16, from the first request:

| Time | Milestone |
|---|---|
| 19:56 | Start. Symptom in hand: `errno: 97` on `::*:6379`, redis "Ready to accept connections" |
| 19:59 | Worktree cut, `devbox-smoltcp` building |
| ~20:04 | First `redis-server` up in a test VM; `redis-cli` cannot reach it |
| ~20:14 | **Connect-redial bug fixed → `redis-cli` gets `PONG`.** apk Redis working: **18 min** |
| ~20:21 | Bind-port-0 bug fixed; `busybox nc` works too |
| ~20:33 | `box pull redis:alpine` — 7 layers off Docker Hub, first try |
| ~20:37 | **Official image serving, host-reachable: 41 min** |
| ~21:14 | Its own `docker-entrypoint.sh` gets as far as `setpriv` starting Redis |

So: **~18 minutes to the Alpine package, ~41 minutes to the official Docker
image**, on a kernel where the same program could not start at all four days
earlier. The long pole was neither: it was the following hour spent on the
container entrypoint's privilege drop, which is still unfinished (§6).

That number is only meaningful against what it took to get *this* far. The
prerequisite work — `/proc/<pid>/{cmdline,status,stat}`, `MADV_FREE` honesty,
the OCI pull pipeline, overlayfs, fork/CoW — is measured in weeks, and is why
41 minutes was possible.

## 2. The two `akuma-net` bugs

Both presented identically: **`Connection refused` from a client on the same
box, against a listener that was up and healthy.** `/proc/net/tcp` showed the
listener in `LISTEN`. Traffic from *outside* the VM worked the whole time —
`nc` from the host through QEMU's port forward got `+PONG` before either fix.
That asymmetry is what made it look like a loopback problem. It was not;
loopback was fine.

### 2.1 A redial was reported as ECONNREFUSED

`socket_connect` handed every `connect(2)` straight to
`smoltcp::tcp::Socket::connect`, which rejects any socket that is not `Closed`
with `ConnectError::InvalidState` — and the error mapping collapsed that to
`ECONNREFUSED`.

That matters because of the standard non-blocking idiom:

```
connect()  -> EINPROGRESS
poll()     -> writable
connect()  -> "did it work?"
```

hiredis — so `redis-cli`, and anything else built on it — does exactly this.
The kernel log tells the whole story:

```
[syscall] connect(fd=3, ip=127.0.0.1:4444)
[syscall] connect(fd=3) = EINPROGRESS
[syscall] connect(fd=3, ip=127.0.0.1:4444)
[syscall] connect(fd=3) = err 111      <- before
[syscall] connect(fd=3) = OK           <- after
```

The fix classifies the socket's state first (`connect_step`): `Established` →
success, `SynSent`/`SynReceived` → `EALREADY` for a non-blocking caller or wait
for completion *without re-issuing the SYN* for a blocking one, anything else →
dial.

### 2.2 `bind(0.0.0.0:0)` on TCP stored port 0

Port 0 means "pick one for me". The UDP arm of `socket_bind` allocated an
ephemeral port; the TCP arm stored the literal `0`. The next `connect` then
passed smoltcp `local_port = 0`, which it rejects as `Unaddressable` — again
reported as `ECONNREFUSED`. Any client that binds before connecting hit it;
`busybox nc` does.

### 2.3 The reason both hid: one errno for every failure

Every non-`Established` outcome of `connect` returned `ECONNREFUSED` —
including the 10-second timeout. "Nothing is listening", "the connect never
completed" and "the local address is unusable" were indistinguishable from
userspace, and each fix's failure looked exactly like the bug it was meant to
fix.

Splitting them (`connect_outcome`) is what found §2.2 in a single run: after
the first fix, `nc` still failed, but now with `err 99` = `EADDRNOTAVAIL`,
which points at exactly one line of code. New errnos in
`akuma_primitives::errno`: `EADDRNOTAVAIL`, `EISCONN`, `EALREADY`.

**Method note.** The temptation was to debug this by reasoning about the
loopback device — the symptom was "only local clients fail", the stack has a
hand-rolled `LoopbackAwareDevice`, and 127.x frame interception is exactly the
sort of thing that would be subtly wrong. An hour could have gone there. What
actually settled it was making the kernel say *which* failure it was, which
took one 20-line change and one rebuild.

Both fixes are pure-function-tested (`connect_state_tests` in
`crates/akuma-net/src/tests.rs`) — the state machine and the errno mapping are
extracted from the smoltcp calls specifically so they can be tested without a
network.

## 3. What the official image needed

`box pull redis:alpine` worked first try — auth, manifest-list arm64
resolution, 7 layers downloaded and extracted. `box run` did not, and each
failure exposed a real gap:

| Symptom | Gap | Fix |
|---|---|---|
| `box run: failed to spawn /usr/local/bin/docker-entrypoint.sh` | `sys_spawn` had no `#!` handling — only `do_execve` did. Every official image's Entrypoint is a shell script | `resolve_shebang_chain` in `crates/akuma-exec/src/process/spawn.rs`, resolved *inside* the namespace override so a container's own `/bin/sh` is the one found |
| `exec: line 184: redis-server: not found` | `DEFAULT_ENV`'s `PATH` was `/usr/bin:/bin`. Images install under `/usr/local/bin` | Full Linux search order in `DEFAULT_ENV` |
| `setpriv: getresuid failed: Function not implemented` | `getresuid`/`getresgid`/`getgroups` were ENOSYS | Implemented; everything is root, so all report 0 |
| `setpriv: activate capabilities: No error information` | Two causes, in order — see §4 | `/proc/self/…` path resolution, then real `capget` version negotiation |

### The argv[0] bug found along the way

Independently, `exec_shebang` in `src/syscall/proc.rs` was shadowing the
interpreter-as-written with its symlink-resolved target and using the resolved
path as `argv[0]`. Linux uses the name from the `#!` line. This is not cosmetic
on a busybox system: busybox is a multi-call binary that dispatches **entirely**
on `argv[0]`, so `#!/bin/sh` ran `/bin/busybox` with `argv[0]="/bin/busybox"`
and busybox had no idea it was supposed to be a shell.

Both paths now share one parser and one argv rule (`parse_shebang`,
`shebang_hop`), because two implementations of one rule is how they came to
disagree in the first place.

## 4. `setpriv: activate capabilities: No error information`

Worth recording as a debugging story, because the message is actively
misleading and the first fix was wrong.

`No error information` is musl's `strerror(0)`: the failing call returned -1
**without setting errno**, which means it was not a syscall. It was libcap-ng.

The first guess was `capset(2)`, which was indeed missing. Stubbing it to
succeed changed nothing. The second guess was that libcap-ng reads capabilities
out of `/proc/self/status` — so `CapInh`/`CapPrm`/`CapEff`/`CapBnd` lines were
added. Also nothing. Adding them did expose the real problem:

**`/proc/self/<anything>` did not resolve at all.** `read_symlink` reported
`self` correctly, but the VFS hands procfs the literal path rather than chasing
the symlink, so `/proc/self/status` arrived as the string `self/status` and
matched nothing. `cat /proc/self/status` → `No such file or directory`, in box 0
and in containers alike. This is the same gap that blocked Redis from starting
in the first place four days earlier (`/proc/self/smaps`,
[`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) §3.4) — fixed there by adding
the *files*, never by making `self` work.

With `resolve_self` in `src/vfs/proc.rs` rewriting a leading `self/` to the
caller's pid, `/proc/self/status` works — and setpriv still failed identically.

The actual cause was the third one: **`capget` returned success for any input**.
Linux's `capget` rejects an unknown `hdr.version` by writing back the version it
*does* support and returning `EINVAL` — that is a negotiation, and libcap-ng
performs it by calling `capget` with version 0 to be told which layout to use.
Answering "0 is fine" made every subsequent call use a layout the kernel never
agreed to. Real version negotiation plus a full-root capability set (matching
what procfs now reports) fixed it:

```
~ # box run --rm --entrypoint /bin/sh redis:alpine -c \
      "setpriv --reuid=redis /usr/local/bin/redis-server --version"
Redis server v=8.10.0 sha=00000000:1 malloc=jemalloc-5.3.0 bits=64
```

Three plausible causes, in the order they were tried; only the third was it.
The first two were nonetheless real gaps and are worth keeping — but note that
each one "should have" fixed the symptom, and neither did. Verify the fix, not
the theory.

## 5. What works

Verified on `devbox-smoltcp`, `SMP=4`, `MEMORY=4096`:

- Alpine `redis-server` 8.8.0: `PING`/`SET`/`GET`/`HSET`/`ZADD`/`EXPIRE`/`TTL`,
  `BGSAVE` (fork + a real `dump.rdb`)
- `redis-benchmark -c 20 -n 4000`: SET 4008 rps p50 2.3 ms, GET 4566 rps
- Official `redis:alpine` (Redis 8.10.0) in a box via `--entrypoint`, reporting
  `os:Akuma 0.0.7 aarch64`, reachable from box 0 and from the macOS host
- The host writing a key and the guest reading it back, and vice versa

## 6. The remaining gap: per-process credentials

`docker-entrypoint.sh` under its own Entrypoint still does not reach
`redis-server`. Not a crash — an **infinite re-exec loop**:

```sh
if [ "$1" = 'redis-server' -a "$(id -u)" = '0' ]; then
	find . \! -user redis -exec chown redis '{}' +
	exec setpriv --reuid=redis --regid=redis --clear-groups -- "$0" "$@"
fi
exec "$@"
```

`setpriv` now succeeds — but `setresuid` is an accepting no-op and `getuid`
hardcodes 0, so on the re-exec `id -u` is *still* 0, the branch is taken again,
and the script re-execs itself under `setpriv` forever. It never reaches
`exec "$@"`.

This is the cost of a silently-succeeding credential syscall, and it is a
general trap rather than a Redis one: the "drop privileges and re-exec" shape is
in most official images' entrypoints.

**The fix is per-process credentials**, and it does not require enforcement —
only bookkeeping. Add `uid`/`gid` to `Process`, have
`getuid`/`geteuid`/`getgid`/`getegid`/`getresuid`/`getresgid` read them and
`setuid`/`setresuid`/`setgid`/`setresgid` write them, inherit across
fork/exec. No permission check anywhere — the kernel would simply report the
identity a process asked for, which is enough to break the loop. Estimated at a
couple of hours; the risk is not the mechanism but the second-order effects
(tools that behave differently as non-root, and file permission checks the
kernel does not enforce).

Until then: `--entrypoint /usr/local/bin/redis-server` skips the script.

## 7. Things noticed but not chased

- **Socket budget.** `redis-benchmark -c 50` fails with `Can't create socket:
  No file descriptors available`. Each listener pre-allocates `MAX_BACKLOG`
  (32) smoltcp sockets at 32 KB, and closed sockets sit in `pending_removal`
  before their slots return. Back-to-back runs exhaust it. Fix would be a lazy
  backlog plus a configurable cap.
- **`sendfile` (nr 71) is ENOSYS**, called as `sendfile(1, 3, NULL)` — a
  file-to-stdout copy — a couple of times per container run. Harmless; callers
  fall back to read/write.
- **`INFO persistence` right after `BGSAVE`** once returned `recv timeout`
  while the RDB wrote correctly. Once, not reproduced.
- **`Protocol error, got "<c>" as reply type byte`** — first seen here as a
  one-off and written up as unexplained. It was not a fluke: **ROOT-CAUSED and
  FIXED** the same day once it reproduced on `KEYS *` against a populated
  database, from the host *and* from inside the VM. `sys_writev` did not stop at
  a short write, so every reply larger than smoltcp's 16 KB TX window came out
  of the socket spliced. Full A/B and mechanism:
  [`WRITEV_SHORT_WRITE_SPLICE.md`](WRITEV_SHORT_WRITE_SPLICE.md).

  Worth noting how the first sighting was mis-scoped here: the probe written for
  it (`scripts/redis_stream_integrity.py`) sends `PING` and checks `+PONG\r\n`
  — 7 bytes. It passed 700 connections cleanly and was taken as evidence of no
  corruption, when it could not have detected this bug at all. **A negative
  result is only as strong as the size of the thing it exercised.**

- **No IPv6 anywhere.** The `errno: 97` line Redis prints at startup is that,
  and is harmless (`DEVBOX_ISSUES.md` Issue 9).

## Background

- [`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) — why `redis-server` could
  not start at all before 2026-08-12, and the `/proc/self/smaps` root cause.
- [`../runbooks/run-redis.md`](../runbooks/run-redis.md) — the recipes.
- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) — Issue 9 (IPv6), 14 (shebang in
  spawn), 15 (credentials), 16 (socket budget).
- [`../../userspace/box/docs/OCI_IMAGE_PULL.md`](../../userspace/box/docs/OCI_IMAGE_PULL.md)
  — the pull pipeline that made §3 possible.
