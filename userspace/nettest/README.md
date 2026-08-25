# nettest — guest-side network client probes

Five probes live here. They exist for four different investigations, share
nothing but a directory, and are built by two different scripts.

| Probe | Directory | Client stack | Investigation | Build |
|---|---|---|---|---|
| `nettest` | `rust/` | libcurl (vendored, static OpenSSL + nghttp2) | cargo-vs-curl HTTPS divergence | `rust/build.sh` (Alpine docker) |
| `nettest parkprobe` (mode) | `rust/` | fork/waitpid/kill + blocking TCP, no curl | SMP=1 `idle_halt` scheduler freeze acceptance ([`SECOND_LISTENER_SMP1_FREEZE.md`](../../docs/archive/SECOND_LISTENER_SMP1_FREEZE.md)) | `rust/build.sh` (Alpine docker) |
| `nettest-connect` | `rust/connect/` | raw `connect(2)` + `poll`/`select`/`epoll` — no library at all | cargo-vs-curl HTTPS divergence | `rust/build-musl.sh` (host cross) |
| `nettest-std` | `rust/stdlib/` | `std::net` + `poll(2)` + sync rustls — no runtime | delayed first byte | `rust/build-musl.sh` (host cross) |
| `nettest-reqwest` | `rust/reqwest/` | tokio + hyper 1.x + reqwest 0.12 + rustls — nca's stack | delayed first byte | `rust/build-musl.sh` (host cross) |
| `nettest-unix` | `rust/unixsock/` | raw AF_UNIX syscalls — no `std::os::unix::net` | AF_UNIX implementation | `rust/build-musl.sh unix` (host cross) |

---

# Part 1 — `nettest-std` / `nettest-reqwest`: the delayed-first-byte bisect

A guest client hangs when the server takes more than a few seconds to send its
**first** response byte, while the identical request answered within ~1 s
streams perfectly
([`docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../../docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md)).
The only client that reproduced it was nca — tokio + hyper + reqwest + rustls +
an agent loop — so the investigation could not say *which layer* stalls.

**Outcome (2026-08-17): four kernel defects found and fixed.** A blocking TCP
read capped at 30 s and a write at 5 s (spurious `ETIMEDOUT`);
`SO_RCVTIMEO`/`SO_SNDTIMEO` accepted and silently dropped; the `EPOLLET` write
edge never re-armed; and — the dominant one — a socket still in `SynSent`
reported as read-closed (`EPOLLIN` + `EPOLLRDHUP`, `recv() == Ok(0)`), which
made a tokio client park forever without ever sending its request. Details in
[`docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../../docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md)
§ Resolution. The probes stay as the regression harness — in particular
`nettest-reqwest post <url> 64` repeated a dozen times, which is what caught the
two races.

These two probes cut that stack into axes that can be tested one at a time:

| probe / mode | sockets | HTTP | TLS | isolates |
|---|---|---|---|---|
| `nettest-std raw` | blocking `std::net` | hand-rolled | none | the kernel's blocking recv path (`socket_recv` → `wait_until`) |
| `nettest-std poll` | nonblocking + `poll(2)` | hand-rolled | none | readiness reporting without epoll (`sys_ppoll`) |
| `nettest-std tls` | blocking `std::net` | hand-rolled | rustls (sync) | rustls without an async runtime |
| `nettest-reqwest get` | tokio/mio + `epoll_pwait` | hyper 1.x | rustls (async) | nca's whole stack |

Both print the same `[probe]` line vocabulary, so their output diffs directly.

## Build and run

```bash
# host: cross-build both -> bootstrap/bin/nettest-{std,reqwest}
userspace/nettest/rust/build-musl.sh
scripts/populate_disk.sh                    # -> /bin in the image

# host: the timing server the probes measure against
scripts/net_delay_server.py --port 18080 --verbose
```

In the guest (`10.0.2.2` is the host over SLIRP — no `hostfwd` rule needed):

```
nettest-std     sweep    http://10.0.2.2:18080          # delay ladder, blocking
nettest-std     sweep    http://10.0.2.2:18080 0,5,35 poll
nettest-std     gap      http://10.0.2.2:18080 0 20     # first byte fast, 20 s mid-stream idle
nettest-std     rcvtimeo http://10.0.2.2:18080/delay/30 2
nettest-std     tls      https://example.com/
nettest-reqwest sweep    http://10.0.2.2:18080
nettest-reqwest stream   http://10.0.2.2:18080/sse/1/10
nettest-reqwest post     http://10.0.2.2:18080/delay/10 64
```

Both probes also build for the **development host**
(`cargo build --release --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)"`).
Run them there against the same delay server first: that is the control. A
sweep that fails in the guest and passes on the host localises the fault to the
kernel; one that fails on both is a probe or server bug.

The step-by-step procedure, the result matrix, and the kernel traces to collect
are in
[`docs/runbooks/debug-delayed-first-byte.md`](../../docs/runbooks/debug-delayed-first-byte.md).
The audit these probes were designed against — poll drivers, RX buffering,
readiness predicates, and the kernel's undeclared timeouts — is
[`docs/reference/subsystems/networking.md`](../../docs/reference/subsystems/networking.md)
§ "The native data path".

## Why these two are cross-built on the host, not in docker

`rust/build-musl.sh` uses `aarch64-unknown-linux-musl` +
`aarch64-linux-musl-gcc` — the **same toolchain `userspace/nca` uses for nca
itself** (`userspace/nca/build.rs`). That is the design constraint, not a
convenience: "nca hangs but the probe does not" is only informative if the two
binaries came out of the same compiler against the same libc. The `reqwest`
probe's dependency line is copied verbatim from nca's
`[workspace.dependencies]` for the same reason.

The curl probe below has the opposite requirement (match apk/nightly cargo's
libcurl), which is why it keeps its own container build.

---

# Part 2 — `nettest`: the cargo-vs-curl HTTPS divergence probe

Why this exists, what it tests, and how to run it.
The root-cause analysis lives in
[`docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../../docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md).

## TL;DR

`cargo fetch` inside a devbox-smoltcp VM fails with
`[7] Could not connect to index.crates.io:443 after ~300 ms` while `curl https://index.crates.io`
in the same shell returns 200 in ~300 ms. The probe distills cargo's exact libcurl
client into a 4-mode binary so we can bisect what cargo does that curl does not.

## Why a probe

cargo's HTTPS path (master `rust-lang/cargo`, `src/cargo/sources/registry/http_remote.rs`
→ `src/cargo/util/network/http_async.rs` + `http.rs`) is more than just "use libcurl":

| Thing cargo does                                       | curl CLI does     |
|--------------------------------------------------------|-------------------|
| `curl::multi::Multi` driven from a worker pthread      | single Easy handle in main thread |
| `multi.pipelining(false, /*multiplex=*/true)`          | no Multi at all |
| `multi.set_max_host_connections(2)`                    | n/a |
| per-handle `http_version(HttpVersion::V2)`             | HTTP/1.1 unless `--http2` |
| per-handle `pipewait(true)`                            | off |
| apk libcurl + OpenSSL + nghttp2 + c-ares               | static mbedTLS (in `/bin/curl`) |

The four modes toggle these axes independently so a single run tells you which axis
triggers the kernel bug.

## Build

```bash
# Host: docker (Alpine arm64). Produces bootstrap/bin/nettest.
userspace/nettest/rust/build.sh
```

`nettest` is a Linux/musl binary, NOT a no_std kernel binary, so it is **not** a member
of `userspace/Cargo.toml`'s workspace. The standalone `[workspace]` table in
`userspace/nettest/rust/Cargo.toml` and the `.cargo/config.toml` override are load-bearing
— without them the build inherits the kernel target (`aarch64-unknown-none`) and fails
to find `std`.

The build links dynamically against apk's `libcurl.so.4` + `libssl.so.3` +
`libnghttp2.so.14` + `libcares.so.2` — the exact sonames apk-installed cargo links
against inside the VM. This is intentional: a statically-bundled libcurl would defeat
the comparison.

## Run (inside the VM, after `populate_disk.sh` has shipped the binary to `/bin/nettest`)

```
# baseline — should always work (mirrors /bin/curl CLI)
nettest easy11
nettest easy11 https://index.crates.io/config.json

# cargo-pattern variants
nettest easy2    https://index.crates.io/config.json
nettest multi11  https://index.crates.io/config.json
nettest multi2   https://index.crates.io/config.json   # cargo's exact setup

# big payload — compare against the user's curl-downloaded flac
nettest easy2    https://example.com/big-file.bin
nettest multi2   https://example.com/big-file.bin
```

Every mode prints `[nettest] mode=… OK status=… body=…B perform=…s total=…s` on success
or `[nettest] mode=… FAIL after …s: <curl error>` on failure. libcurl verbose output
(`* Trying IP:port…`, `* SSL connection using TLS…`, `* CONNECTED …`) goes to stderr
so you can watch exactly where each mode dies.

## Reading the results

| `easy11` | `easy2` | `multi11` | `multi2` | Diagnosis |
|----------|---------|-----------|----------|-----------|
| OK       | OK      | OK        | **FAIL** | Hypothesis H2 confirmed: HTTP/2 multiplexing specifically triggers the kernel bug. The existing `CARGO_HTTP_MULTIPLEXING=false` workaround is the correct fix. |
| OK       | OK      | **FAIL**  | **FAIL** | H1/H3/H4 confirmed: any Multi+worker pattern breaks, regardless of multiplexing. The `CARGO_HTTP_MULTIPLEXING=false` workaround is incomplete; the bug is in the kernel's `Multi::wait` / `poll` / worker-thread path. |
| OK       | **FAIL**| **FAIL**  | **FAIL** | HTTP/2 itself is the trigger (TLS ALPN h2). Multiplexing is downstream of that. |
| OK       | OK      | OK        | OK       | Probe did not reproduce — try larger payloads or repeat the failing `cargo fetch` and compare verbose output. |

See [`docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../../docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md)
§ "Hypothesis" for the full H1–H4 definitions.

## What to capture when it fails

For each failing mode, grab:

1. The probe's stdout/stderr (it carries libcurl's `*`-prefixed verbose trace).
2. The kernel serial log around the failure — filter for
   `[syscall] connect(fd=N, ip=A.B.C.D:443)` and the matching `= OK` / `= -ERR` line.
   `src/syscall/net.rs:306` is the log site.
3. `curl -v https://index.crates.io/config.json` from the same shell, for the diff.

That triad is enough to identify which hypothesis above is correct.

---

# Part 3 — `nettest-connect`: what the connect actually reports

Same investigation as `nettest` (Part 2), one layer lower. Part 2 asks "does a
libcurl shaped like cargo's fail?"; this asks "what does the kernel answer, and
what would libcurl conclude from that answer?". It links nothing — `libc` and
raw syscalls only.

## Why a fourth probe

`docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md` concludes that nightly cargo's
non-blocking connects "never complete". libcurl gives up after **~353 ms**,
which no hung connect can explain — `CURLOPT_CONNECTTIMEOUT` is 300 s and
cargo's `CARGO_HTTP_TIMEOUT` is 30 s. Reading the exact libcurl that cargo is
built from settles what the give-up requires. In
`curl-sys-0.4.90+curl-8.21.0/curl/lib/cf-socket.c`, `cf_tcp_connect()`:

```c
rc = SOCKET_WRITABLE(ctx->sock, 0);          /* poll(POLLOUT), timeout 0 */
if(rc == 0)                     /* "not connected yet" — attempt stays ONGOING */
else if(rc == CURL_CSELECT_OUT) /* verifyconnect(): getsockopt(SO_ERROR) */
else if(rc & CURL_CSELECT_ERR)  /* HARD FAIL */
```

and `cf-ip-happy.c` raises `CURLE_COULDNT_CONNECT` — the observed `[7] Could not
connect to server` — **only when no attempt is ongoing any more**. So the
failing attempts are being *refused*, not ignored, and there are exactly two
ways to refuse them:

| what `poll` reports on the connecting fd | libcurl concludes |
|---|---|
| `0` | still connecting (attempt stays ongoing — cannot produce the 353 ms error) |
| `POLLOUT`, `SO_ERROR == 0` | connected |
| `POLLOUT`, `SO_ERROR != 0` | hard fail |
| anything with `POLLERR` / `POLLHUP` / `POLLNVAL` / `POLLPRI` | hard fail |

Note the `==` in the `CURL_CSELECT_OUT` test: `POLLOUT|POLLHUP` together take
the error branch too.

Kernel side, for reading the output: `epoll_check_fd_readiness`
(`src/syscall/poll.rs`) raises `EPOLLHUP` whenever `socket_is_dead_tcp()` — i.e.
whenever smoltcp's `is_active()` is false, which is `Closed`, `TimeWait` or
`Listen` — and `sys_ppoll` passes `POLLHUP` through regardless of the requested
mask. `POLLOUT` needs `can_send()`, true only in `Established`/`CloseWait`. A
socket in `SynSent` therefore polls as `0`, which is correct and is *not* a
fast-fail state.

## Build and run

```bash
userspace/nettest/rust/build-musl.sh connect   # -> bootstrap/bin/nettest-connect
scripts/populate_disk.sh                       # -> /bin/nettest-connect in the image
```

```text
nettest-connect resolve <host>              # getaddrinfo only, with timing
nettest-connect one     <host> <port>       # one attempt, full syscall timeline
nettest-connect all     <host> <port>       # one attempt per resolved address
nettest-connect he      <host> <port>       # cargo's happy-eyeballs, emulated
nettest-connect churn   <host> <port> <n>   # n sequential attempts, histogram
```

Flags: `--wait poll0|poll|select|epoll` (default `poll0`, libcurl's own
zero-timeout query), `--timeout-ms N` (5000), `--sample-ms N` (1),
`--nonblock fcntl|sockflag` (default `fcntl`, what libcurl does),
`--soerr-every-sample`, `--quiet`.

`he` is the mode that matters: it re-implements `cf_ip_ballers_run()` —
one attempt per address, a new one every 200 ms
(`CURLOPT_HAPPY_EYEBALLS_TIMEOUT_MS`), at most 6 alive with the oldest pruned,
`COULDNT_CONNECT` only when the list is exhausted and nothing is ongoing. If it
prints `COULDNT_CONNECT` at a few hundred ms it has reproduced cargo's failure
with no libcurl in the picture.

## Reading it

| verdict | meaning |
|---|---|
| `CONNECTED` | `POLLOUT` with `SO_ERROR == 0` |
| `HARDFAIL_POLLERR` | `POLLERR`/`POLLHUP`/`POLLNVAL`/`POLLPRI` — libcurl aborts this address |
| `HARDFAIL_SOERROR` | `POLLOUT` but `SO_ERROR != 0` — libcurl aborts this address |
| `PENDING` | still `SynSent` at `--timeout-ms` — what "connects never complete" would actually look like |
| `CONNECT_FAILED` | `socket()`/`connect()`/the readiness syscall itself errored |

A `HARDFAIL_*` at ~one RTT reproduces the symptom and retires the "never
complete" wording. A `PENDING` at 5 s confirms the wording and moves the
contradiction into libcurl's timing. Either result closes a gap.

The four `--wait` modes are a bisect of the readiness path, not redundancy:
`poll0`/`poll` → `sys_ppoll`, `select` → `sys_pselect6`, `epoll` →
`sys_epoll_pwait`. `poll0` never blocks and reads pure level state, while `poll`
blocks — readiness that appears only when someone waits is a lost-wake bug,
readiness that never appears is something else. `sys_pselect6` translates only
`EPOLLIN`/`EPOLLOUT` and drops `EPOLLERR`/`EPOLLHUP`, so a socket that polls as
`HUP` is *expected* to select as nothing; that asymmetry is part of the matrix.

## The Linux control arm

The binary is static `aarch64-unknown-linux-musl`, so the same file runs under
Docker Linux (`docs/archive/LINUX_AB_PROBE.md`). Reference output there:

```text
[probe] RESULT 146.75.122.137:443 verdict=CONNECTED t=66.9ms so_error=0 revents=OUT|WRNORM
[probe] RESULT 127.0.0.1:9        verdict=HARDFAIL_POLLERR t=0.0ms so_error=111 ECONNREFUSED revents=OUT|ERR|HUP|WRNORM
[probe] RESULT 10.255.255.1:443   verdict=PENDING t=701.7ms so_error=0 revents=0
```

All four wait modes agree there. A verdict that differs between Linux and the
guest is a kernel divergence; one that matches is libcurl's or the network's.

## Hazard: `SO_ERROR` is consuming on Linux

`getsockopt(SO_ERROR)` clears the pending error on Linux; Akuma recomputes it
from smoltcp state and clears nothing (`sys_getsockopt`, `src/syscall/net.rs`).
The probe therefore reads it only where libcurl does — once, at the verdict —
unless `--soerr-every-sample` is passed. Sampling it every iteration makes the
Linux arm lie.

## Outcome: the bug this probe found (2026-08-20)

`nettest-connect` isolated it to a single syscall in one run, same address, same
moment:

```text
nettest-connect one index.crates.io 443 --wait poll     t=77.0ms revents=OUT      -> CONNECTED
nettest-connect one index.crates.io 443 --wait select   t=68.6ms revents=PRI|OUT  -> HARDFAIL
```

`sys_pselect6` took `_exceptfds_ptr` and never wrote it, so the caller's
exceptional-condition set came back with every fd still flagged. The nightly Rust
toolchain's libcurl compiles `Curl_poll()`'s `select(2)` branch (curl-sys'
`build.rs` defines `HAVE_POLL_H`/`HAVE_POLL_FINE` but not plain `HAVE_POLL`) and
asks for `POLLPRI` on a connecting socket, which that branch puts in `exceptfds`
— so it read the stale set back as `POLLPRI`, mapped it to `CURL_CSELECT_ERR`,
and discarded every socket about one RTT *after* it reached `Established` with
`SO_ERROR == 0`. Fixed; `cargo fetch` now runs 3/3 cold with zero spurious
errors. Full write-up:
[`docs/runbooks/cargo-cannot-reach-crates-io.md`](../../docs/runbooks/cargo-cannot-reach-crates-io.md) § 3.

The same run also closed a second gap: a `SynSent` socket could never time out,
because nothing set smoltcp's `timeout` and smoltcp retransmits a SYN forever.
`CONNECT_TIMEOUT_US` (10 s, matching the blocking path's existing cap) now bounds
it, scoped to `SynSent` so idle established connections are untouched.

## What the source trace established (2026-08-20)

Traced from `cf_tcp_connect()` down to smoltcp and back. Two results worth
knowing before you read a run:

**A `SynSent` socket could not die on its own** (true until the
`CONNECT_TIMEOUT_US` sweep was added on 2026-08-20; the reasoning is what made
the `revents` table below decisive). Nothing in the tree calls smoltcp's
`set_timeout()` (repo-wide: zero hits). The field defaults to `None`
(smoltcp-0.12.0 `src/socket/tcp.rs:527`) and `timed_out()` is hardcoded false
while it is `None` (`tcp.rs:2118`), so an unanswered SYN retransmits forever —
it never reaches `Closed`. The only `close()`/`abort()` callers on a TCP handle
are `remove_socket()` (last fd closed, `crates/akuma-net/src/socket.rs:438`),
the `poll()` GC sweep (`smoltcp_net.rs:961`) and `reclaim_pending_slots()`
(`smoltcp_net.rs:1201`) — and the latter two only touch handles already parked
in `pending_removal`, i.e. whose last fd is gone. `dup`/`dup2`/`F_DUPFD`/fork
all refcount through `socket_clone_ref` (`src/syscall/fs.rs:1360`, `:1395`,
`:2360`).

So a `POLLHUP` on a *live connecting fd* within one RTT has only three
possible origins, and `revents` alone tells them apart — which is why the probe
prints a `hint:` line:

| `revents` | origin |
|---|---|
| `HUP` alone | smoltcp reached `!is_active()` — an RST arrived, or a recycled handle |
| `ERR\|HUP` | `current_process_shared()`/`get_fd()` returned `None` (`src/syscall/poll.rs:466`); no socket state was consulted at all |
| `OUT` with `ERR`/`HUP` | impossible on Akuma — you are looking at the Linux arm |

**Akuma never reports `POLLOUT` together with `POLLHUP` for a socket.**
`epoll_check_fd_readiness` takes `EPOLLHUP` *or* the `IN`/`OUT`/`RDHUP` branch,
never both (`src/syscall/poll.rs:498-510`). Linux reports `OUT|ERR|HUP|WRNORM`
for a refused connect; Akuma can only report `HUP`. libcurl's verdict is the
same either way (`cf_tcp_connect` tests `rc == CURL_CSELECT_OUT` by equality),
but do not mistake the differing bits for the bug. It also means
`HARDFAIL_SOERROR` is effectively unreachable on Akuma: the hard failure always
arrives through the `CURL_CSELECT_ERR` branch.

## Correction to Part 2's build description

Part 2 above and `docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md` both describe
`nettest` as linking apk's shared libcurl and name "rebuild it with
`static-curl`" as the next step. That text is stale. `rust/Cargo.toml` already
carries `features = ["http2", "ssl", "static-curl", "static-ssl"]`, and the
built binary (`rust/nettest`, gitignored; a copy is in `bootstrap/bin/nettest`)
contains vendored `curl/lib/vtls/openssl.c` + nghttp2 sources and the string
`OpenSSL 3.6.3` — the same vendored OpenSSL the nightly toolchain reports.
It was built at 20:19 on 2026-08-11, twenty minutes *before* the commit that
added the doc (`0c9d96ce`, "curious case of nothingburger"). The vendored probe
exists and has shipped; what is unrecorded is whether the 30/30 pass was
measured with it or with the earlier dynamic build.

---

# Part 4 — `nettest-unix`: does AF_UNIX work, and does it work the way Linux does?

Until 2026-08-23 Akuma had no AF_UNIX socket object at all: `socket(AF_UNIX, …)`
returned `EAFNOSUPPORT`, and the only thing that worked was `socketpair(2)` over
two kernel pipes. The audit, the plan and the outcome are in
[`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md`](../../docs/archive/UNIX_SOCKET_IMPROVEMENTS.md).

## Why a probe, and why the Linux arm is not optional here

Two of the defects that audit found were **silent**: `SOCK_SEQPACKET` merged
messages, and `sendmsg` sent only the first iovec. Neither produces an error, an
errno, or a kernel log line — the caller gets a plausible short count and carries
on with corrupt data. Every mode below is written against one specific way to
lose or duplicate user data, and each reports *which* way it failed rather than
just that it did.

The Linux control arm matters more for this probe than for the four above, and
for a structural reason. A unix-socket probe is **entirely self-contained**: no
server, no network, no peer to blame. So there is no external reference to
disagree with, and running the identical static-musl binary under Docker Linux is
the *only* way to tell a kernel bug from a probe bug.

That is not a theoretical benefit. The Linux arm found **five kernel defects and
two bugs in this probe**, including one where the probe's own assertion was wrong
in the same direction as the kernel (`SHUT_RD`) — a test written from the same
misunderstanding as the implementation would have passed. See § 0 of the audit
doc for the list.

**Run the Linux arm first. It is free, and a mode that fails there is a probe
bug.**

## Build and run

```bash
userspace/nettest/rust/build-musl.sh unix   # -> bootstrap/bin/nettest-unix
scripts/populate_disk.sh                    # -> /bin/nettest-unix in the image
```

```text
nettest-unix all                        # every argument-free mode, in phase order
nettest-unix pair stream|seqpacket      # socketpair, TWO messages each way
nettest-unix iovec                      # sendmsg 3 iovecs / recvmsg into 2
nettest-unix shutdown                   # the SHUT_RD / SHUT_WR / bad-how matrix
nettest-unix abstract                   # bind/listen/connect/accept, no VFS
nettest-unix path  /tmp/p.sock          # the same over a path, plus S_ISSOCK
nettest-unix stale /tmp/s.sock          # crashed-daemon node: refuse, then re-bind
nettest-unix dgram /tmp/d.sock          # SOCK_DGRAM sendto + a zero-length datagram
nettest-unix syslog                     # connect(/dev/log) and send one line
nettest-unix passfd                     # SCM_RIGHTS — the passed fd must WORK
nettest-unix peercred                   # SO_PEERCRED reports our own pid
nettest-unix poll                       # readiness via poll AND select AND epoll
nettest-unix stress 200                 # n connect/accept/close cycles + fd leak check
```

The Linux arm, against the same binary:

```bash
docker run --rm --platform linux/arm64 -v "$PWD/bootstrap/bin:/b:ro" \
    alpine:3.20 /b/nettest-unix all
```

## Reading the verdicts

| verdict | meaning |
|---|---|
| `OK` | every assertion in the mode held |
| `UNSUPPORTED` | a syscall returned `EAFNOSUPPORT`/`ENOSYS`/`EOPNOTSUPP` — not built yet, **not** broken |
| `TRUNCATED` | data arrived short, or a message boundary was lost — the silent class |
| `LEAK` | an fd count did not return to baseline |
| `READINESS` | poll/select/epoll disagreed about the same fd at the same instant |
| `FAIL` | a syscall failed where Linux succeeds |

A verdict that differs between the Linux arm and the guest is a kernel
divergence. One that matches on both is not a bug.

`UNSUPPORTED` is deliberately not a failure: a phase that has not landed should
not read as a regression. The corollary bit the probe once — `mode_path`
originally treated `Unsupported` as "keep going", so on the rump build it
continued past a rendezvous that had never created the socket and reported
**OK**. `Verdict::succeeded` (did it happen) is now distinct from
`Verdict::is_acceptable` (is this an acceptable outcome).

## Why `poll` checks all three readiness syscalls

`poll`, `select` and `epoll` are separate kernel paths (`sys_ppoll`,
`sys_pselect6`, `sys_epoll_pwait`), and the AF_INET version of exactly this
disagreement was a real bug — `poll` said `CONNECTED` and `select` said
`HARDFAIL` for one socket at one instant, because `sys_pselect6` never wrote
`exceptfds` (Part 3, "Outcome"). A listener's `EPOLLIN` is a brand-new
predicate: a listening unix socket has **no pipes at all**, so a path that falls
through to the pipe arms asks `pipe_can_read(0)` — false for a pipe that does not
exist — and an accept-ready listener polls as "nothing" forever. Every
event-loop server would hang at startup. The mode also asserts an *idle*
listener is **not** readable, because one that always reports ready spins an
event loop at 100% CPU on an `accept` that returns `EAGAIN`.

## Why `stress` exists

Every leak class in the AF_UNIX design is *accumulating* and invisible from
userspace: a name left behind by a closed listener (which makes every subsequent
`bind` fail `EADDRINUSE`, so the service can never restart), a server endpoint
queued but never accepted, a channel outliving its pipe. One round trip passes
with any of them present.

## The three build targets it has to agree on

AF_UNIX is smoltcp-free by construction, so it must work on the rump-only devbox
too — and that is the target where the un-gated dispatch in `src/syscall/mod.rs`
is load-bearing:

```bash
cargo build --release && cargo run --release                       # default (smoltcp)
scripts/build_devbox_smoltcp.sh && overlays/devbox/run-smoltcp.sh  # devbox-smoltcp
scripts/build_devbox.sh && overlays/devbox/run.sh                  # rump-only
```

On the rump build the additional acceptance check is that **box 0's rump stack
still comes up** (`[RUMP-SP] box=0 proxy ready`): the sysproxy channel at fd 3 is
a `UnixSocket`, so any regression in the descriptor layout or in `sendmsg`'s
iovec coalescing kills the handshake silently, several layers away from anything
that looks like socket code.
