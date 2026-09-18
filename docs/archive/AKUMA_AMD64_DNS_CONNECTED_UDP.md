# amd64: DNS worked for two resolvers out of three

**Date:** 2026-09-18. **Found by:** `git clone` hanging on the bare-metal box
while setting up [`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md).
**Fixed:** four defects, all on the *connected* UDP path.

---

## The symptom, and why it read as a network problem

```
$ curl -Lv https://github.com/
* Could not resolve host: github.com (Could not contact DNS servers)
curl: (6) Could not resolve host: github.com

$ git clone --depth=1 https://github.com/netoneko/akuma.git
    ... hangs, three processes at 0:00 CPU ...

$ nslookup github.com
Server:  1.1.1.1
Name:    github.com
Address: 20.217.135.5          # ← works perfectly
```

**`nslookup` succeeding is what made this hard**, and it is also the whole
diagnostic. Three programs, three *different* resolvers:

| program | resolver | uses |
|---|---|---|
| busybox `nslookup` | builds its own DNS query | `sendto` on an **unconnected** socket |
| anything calling `getaddrinfo` | **musl** `__res_msend` | `sendto` on an **unconnected** socket, `poll`, `recvfrom` |
| `curl`, `git` | **c-ares** | **`connect`**, then `send`/`recv`, plus `getsockname`/`getpeername` |

Only the third path was broken. So "DNS works" was true of two resolvers and
false of the two programs anybody cared about.

> **The rule this earns:** `nslookup` is not a test of DNS on this kernel. It
> shares no syscall sequence with the libc resolver and none at all with c-ares.
> Testing DNS means testing **the program that failed**, or a probe that replays
> its exact sequence.

`(Could not contact DNS servers)` is `ares_strerror()`, not a curl or musl
string — the parenthetical names the library, which is the fastest way to know
which of the three paths you are in.

## How it was found: replay the sequence, print errno per step

Guessing was going nowhere (three plausible candidates, each a kernel change).
Instead, a ~40-line C probe replays each resolver's syscall sequence and prints
the return and `errno` of every step. Two runs settled it.

**musl's path — all green, including `getaddrinfo` itself:**

```
socket(AF_INET6,DGRAM|NB|CX) = -1  errno=97  → musl falls back only on EAFNOSUPPORT(97): OK
socket(AF_INET,DGRAM|NB|CX)  = 3
bind(INADDR_ANY:0)           = 0
sendto(1.1.1.1:53, 28 bytes) = 28
poll(POLLIN,5000)            = 1   revents=0x1
recvfrom()                   = 44  answers=1
getaddrinfo("github.com")    = 0   OK
```

**c-ares' path — three failures in eight calls:**

```
connect(1.1.1.1:53)          = 0
send()  on connected UDP     = -1  errno=9   Bad file descriptor      ← BUG 2
write() on connected UDP     = 28                                     ← same fd, works
poll(POLLIN,5000)            = 1   revents=0x1
recv()  on connected UDP     = 44
getsockname()                = -1  errno=38  Function not implemented ← BUG 3
getpeername()                = -1  errno=38  Function not implemented ← BUG 4
```

`send()` failing with `EBADF` **while `write()` on the very same descriptor
succeeds** is the line that names the bug. In musl, `send(fd,buf,len,flags)` is
`sendto(fd,buf,len,flags,NULL,0)` — so a null destination was the variable.

## The four defects

### 1. `SOCK_NONBLOCK` / `SOCK_CLOEXEC` were parsed off and discarded

`amd64/src/sock.rs`'s `sys_socket` masked the flag bits out of `type` with
`SOCK_TYPE_MASK` and never applied them, so **every socket was blocking however
it was asked for**. AArch64 has applied both since it was written
(`akuma_syscalls_glue::net::sys_socket`); this was an amd64-only omission.

**Real, and not the cause.** Fixing it made no difference to `curl` — worth
stating because a fix that is genuinely correct and changes nothing is exactly
where an investigation goes wrong. The self-test
(`sock: SOCK_NONBLOCK is applied to the fd`) also asserts a *plain* socket is
**not** marked, or it would pass on a kernel that marks everything.

### 2. `sendto(…, NULL, 0)` on a connected UDP socket fell into the TCP path

The guard was:

```rust
if dest_addr != 0 && akuma_net::socket::is_udp_socket(idx) { …send to dest… }
send(idx, buf, len, …)   // ← UDP with a null dest lands here: the STREAM path
```

so `send(2)` on a connected UDP socket was answered by the TCP sender and came
back `EBADF`. `write(2)` on the same descriptor worked because it goes through
`akuma-syscalls-glue`, which has always consulted `socket::udp_default_peer`.
One descriptor, two answers, depending on which syscall you used.

**The errno mattered as much as the failure.** `EBADF` tells c-ares its own
descriptor bookkeeping is wrong, not that a send failed — the same shape as
`madvise` answering `ENOSYS` where Linux answers `EINVAL`
([`AKUMA_AMD64_MEMORY_CLOSEOUT.md`](AKUMA_AMD64_MEMORY_CLOSEOUT.md) § "the
errno, not the feature"). The fix returns **`EDESTADDRREQ`** when there is no
peer, which is what Linux and glue both answer.

### 3 & 4. `getsockname` / `getpeername` were `ENOSYS`

Not missing code — **a missing table row.** `akuma-syscalls-glue` has
implemented both for as long as AArch64 has had sockets
(`net::dispatch_getsockname`, which serves AF_UNIX too), but
`akuma-syscalls-abi`'s `syscall_table!` had no entry, so amd64 answered `ENOSYS`
for a syscall the kernel could already service. Added as
`Getsockname => GETSOCKNAME = 51, nr::GETSOCKNAME` (x86_64 literal, aarch64 as a
path into `akuma-syscalls-linux`, per the two rules in `CLAUDE.md`), and routed
to glue exactly as `Getsockopt` already was.

## The assumption underneath all of it

`amd64/src/usermode.rs`'s dispatcher carries this comment, and it was correct
when written:

> *"a UDP `sendto` has no peer to fall back on, and musl's DNS resolver never
> `connect()`s its query socket — it addresses every nameserver by hand on each
> `sendto`."*

Every word is true **of musl**. c-ares does the opposite, and c-ares is what
`curl` and `git` use. The defect is not the reasoning; it is that a statement
about *one* libc's resolver was allowed to shape a syscall's behaviour for all
callers.

**Generalisable:** when a comment justifies a narrow implementation by naming
the one caller it was tested against, that name is a prediction. Ask which
*other* libraries reach the same syscall before trusting it — here it was one
library away.

## Verify

`nslookup` is not a verification. Use the programs that failed:

```sh
/root/probes/resprobe2                       # every step, with errno
curl -sS -o /dev/null -w '%{http_code}\n' https://github.com/
git ls-remote --heads https://github.com/netoneko/akuma.git
```

and in the boot suite: `sock: SOCK_NONBLOCK is applied to the fd`.

**Do not wrap these in `timeout`** — the bare-metal root has no `timeout(1)`, and
`/bin/sh: timeout: not found` makes the pipeline's status come from the *next*
command, which reported `git rc=0` for a git that never ran.

## Background

- [`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md) §3.1 — the clone this blocked.
- [`AKUMA_AMD64_BARE_METAL_SELFHOST.md`](AKUMA_AMD64_BARE_METAL_SELFHOST.md) —
  the self-host loop these fixes were built and installed through.
- [`AKUMA_AMD64_MEMORY_CLOSEOUT.md`](AKUMA_AMD64_MEMORY_CLOSEOUT.md) §3 — the
  errno-not-the-feature precedent.
- [`../reference/subsystems/networking.md`](../reference/subsystems/networking.md).
