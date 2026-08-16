# Run Redis on Akuma

Two ways, both verified end-to-end on `devbox-smoltcp` (2026-08-16): the Alpine
package, and **the official `redis:alpine` image pulled from Docker Hub and run
in a box**. Clients can reach either from inside the VM *and* from the host.

If you are demoing this, read §3 — it is the shortest path from a cold repo to
a macOS terminal typing `SET`/`GET` against Redis running in a container on a
bare-metal Rust kernel.

---

## 1. Prerequisites

A devbox-smoltcp kernel and a devbox image:

```bash
scripts/build_devbox_smoltcp.sh
overlays/devbox/run-smoltcp.sh          # DISK=devbox.img, SMP=4, MEMORY=4096
```

**Pick your port before you start.** QEMU forwards a fixed set of guest ports to
the host, and Redis's usual 6379 is *not* one of them. Use **4444**, which is
forwarded (`scripts/cargo_runner.sh`, `P4444_PORT`):

| Guest port | Host port at `INSTANCE=0` | at `INSTANCE=1` |
|---|---|---|
| 22 (ssh) | 2222 | 2322 |
| 4444 | 4444 | 4544 |
| 8080 | 8080 | 8180 |

`INSTANCE=N` shifts every host port by `100*N` so a second VM can run alongside
the first; it also defaults the disk to `snapshot=on`. To run a second VM
against a *writable* copy of the image, clone it first (`cp -c devbox.img
redis.img` is instant on APFS) and pass `DISK=redis.img SNAPSHOT=0` — QEMU takes
an exclusive lock on the backing file, so two VMs cannot share one image even
with `snapshot=on`.

Everything below assumes `INSTANCE=1` (host ssh 2322, host redis 4544). Drop
100 from each for the default instance.

## 2. The Alpine package

```bash
ssh -o StrictHostKeyChecking=no -p 2322 root@localhost
apk add redis                          # two "applet not found" lines are normal
redis-server --port 4444 --protected-mode no --save "" &
redis-cli -p 4444 ping                 # PONG
```

`redis-cli` against `127.0.0.1` works — loopback is real here
(`LoopbackAwareDevice` in `crates/akuma-net/src/smoltcp_net.rs` intercepts
127.x frames). If it reports `Connection refused` against a server that is
plainly up, you are on a kernel from before 2026-08-16; see
[`../archive/REDIS_END_TO_END.md`](../archive/REDIS_END_TO_END.md) §2.

## 3. The official Docker image (the demo)

```bash
# in the VM
box pull redis:alpine                  # ~7 layers from registry-1.docker.io
box images                             # redis-alpine

box run --rm -d \
    --entrypoint /usr/local/bin/redis-server \
    redis:alpine --port 4444 --protected-mode no --save ""
```

Then from the VM:

```
~ # redis-cli -p 4444 info server | grep -E 'redis_version|^os:'
redis_version:8.10.0
os:Akuma 0.0.7 aarch64
```

and from the host — no Redis client needed, RESP is line-oriented enough to type
by hand:

```bash
printf 'PING\r\nSET greeting hello-from-macos\r\nGET greeting\r\nQUIT\r\n' \
  | nc -w 8 127.0.0.1 4544
```

```
+PONG
+OK
$16
hello-from-macos
```

`os:Akuma 0.0.7 aarch64` is the line worth putting on screen: that is an
unmodified upstream Redis binary, from the official image, reporting the kernel
it found itself on.

### Why `--entrypoint`

The image's own Entrypoint is `/usr/local/bin/docker-entrypoint.sh`, which drops
privileges with `setpriv --reuid=redis`. Akuma has no per-process credentials —
`setresuid` is an accepting no-op and `getuid` always answers 0 — so the script's
`[ "$(id -u)" = '0' ]` test is still true on the second pass and it re-execs
itself under `setpriv` **forever**. It never reaches `exec "$@"`, so nothing
listens and nothing is printed.

`--entrypoint` skips the script and runs the binary directly. Everything the
script would otherwise have done that Akuma *can* do now works — the `#!`
resolution, the `PATH` lookup, `getresuid`/`getresgid`/`getgroups`, `capget`
version negotiation and `/proc/self/status` — so the only remaining gap is the
credential change itself. Details and the fix sketch:
[`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) Issue 15.

## 4. What to expect

Measured on `devbox-smoltcp`, `SMP=4`, `MEMORY=4096`, against the Alpine package:

```
$ redis-benchmark -p 4444 -c 20 -n 4000 -t set,get -q
SET: 4008.02 requests per second, p50=2.327 msec
GET: 4566.21 requests per second, p50=2.191 msec
```

`-c 20` is comfortable. **`-c 50` fails** with `Can't create socket: No file
descriptors available` — that is the kernel socket budget, not Redis: each
listener pre-allocates `MAX_BACKLOG` (32 with the default `many-sessions`
feature) smoltcp sockets at 32 KB each, and closed sockets sit in a deferred
`pending_removal` queue before their slots come back. Back-to-back benchmark
runs can exhaust it even below 50 clients; wait ~20 s between runs.

## 5. Noise you can ignore

```
# Warning: Could not create server TCP listening socket ::*:4444: unable to bind socket, errno: 97
```

`errno 97` is `EAFNOSUPPORT`. Akuma has **no IPv6 at all** — smoltcp is built
`proto-ipv4` only and `sys_socket` rejects any domain but `AF_INET`. Redis tries
IPv6 first, is refused, binds IPv4 and serves normally. See
[`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) Issue 9.

```
monotonic: aarch64, unable to determine clock rate
```

Redis probing `cntfrq_el0` reporting; it falls back to `POSIX clock_gettime`
(the next log line says so) and keeps correct time.

## 6. Persistence

`BGSAVE` works — it forks, and fork's fd/CoW handling is sound — and writes a
real `dump.rdb`:

```
~ # redis-cli -p 4444 config set dir /tmp
~ # redis-cli -p 4444 bgsave
Background saving started
~ # ls -la /tmp/dump.rdb
-rw-rw-rw-  1 0 0  166 Aug 16 17:25 /tmp/dump.rdb
```

One unexplained observation, worth watching rather than trusting: an `INFO
persistence` issued immediately after `BGSAVE` once returned `recv timeout`
while the RDB itself wrote correctly. Not reproduced since.

## Verify

A run is healthy when all of these hold:

```bash
# 1. the server is listening, and the kernel agrees
ssh -p 2322 root@localhost 'cat /proc/net/tcp'
#    -> a row `4444,0.0.0.0:0,LISTEN,<box id>`; a non-zero box id means it is
#       the container's listener, not one in box 0

# 2. loopback works from inside
ssh -p 2322 root@localhost 'redis-cli -p 4444 ping'      # -> PONG

# 3. the host can drive it, and the guest sees what the host wrote
printf 'SET k hostvalue\r\nQUIT\r\n' | nc -w 8 127.0.0.1 4544   # -> +OK
ssh -p 2322 root@localhost 'redis-cli -p 4444 get k'     # -> hostvalue

# 4. it is the upstream binary, on this kernel
ssh -p 2322 root@localhost 'redis-cli -p 4444 info server | grep ^os:'
#    -> os:Akuma 0.0.7 aarch64

# 5. no cross-stream corruption under connection churn
scripts/redis_stream_integrity.py --port 4544 --conns 200 --parallel 16
#    -> 200/200 connections returned exactly b'+PONG\r\n'
```

Raise `--parallel` past ~32 and connections start *timing out* — that is the
backlog of §4, not corruption, and the script says which it saw.

Failure modes and where they point:

| What you see | Meaning |
|---|---|
| `Connection refused` from `redis-cli` against a server that is up | Kernel predates the connect-redial fix (2026-08-16). [`../archive/REDIS_END_TO_END.md`](../archive/REDIS_END_TO_END.md) §2 |
| Host `nc` hangs with no `+PONG` | Wrong port. 6379 is not forwarded; use guest 4444 |
| `box run` prints `failed to spawn …docker-entrypoint.sh` | Kernel predates shebang support in `spawn`. Rebuild, or pass `--entrypoint` |
| `box run` starts and then nothing at all happens | The `setpriv` re-exec loop of §3. Use `--entrypoint` |
| `Can't create socket: No file descriptors available` | Socket budget, §4 |
| `Protocol error, got "<c>" as reply type byte` | **FIXED 2026-08-16.** `sys_writev` did not stop at a short write, so any reply bigger than the 16 KB socket TX window came out spliced — `KEYS *` on a populated database was the reliable trigger. If you see it on an older kernel, that is the bug: [`../archive/WRITEV_SHORT_WRITE_SPLICE.md`](../archive/WRITEV_SHORT_WRITE_SPLICE.md) |

---

## Background

- [`../archive/REDIS_END_TO_END.md`](../archive/REDIS_END_TO_END.md) — how each
  of these was root-caused, and the two `akuma-net` bugs behind the "refused"
  symptom.
- [`../archive/LONG_ROAD_TO_REDIS.md`](../archive/LONG_ROAD_TO_REDIS.md) — the
  earlier investigation that unblocked `redis-server` from starting at all.
- [`../../userspace/box/docs/OCI_IMAGE_PULL.md`](../../userspace/box/docs/OCI_IMAGE_PULL.md)
  — how `box pull` works.
- [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) — Issue 9 (no
  IPv6), Issue 14 (shebang in `spawn`), Issue 15 (credentials).
