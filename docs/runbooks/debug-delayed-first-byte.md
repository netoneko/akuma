# A client hangs when the server is slow to answer

Use this when a guest network client works against a fast endpoint and stalls,
hangs, or dies against a slow one — the shape recorded in
[`../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md):
nca talking to host Ollama completes every round trip when the model prefills in
~1 s and blocks forever when it takes ~10 s before the first response byte.

> **Four defects behind this symptom were root-caused and fixed 2026-08-17.**
> If you are here because a client is hanging *now*, first confirm you are on a
> kernel that has them — the boot log must show
> `[Test] socket_timeout_option_roundtrip PASSED` and
> `[Test] epoll_edge_rearm_symmetry PASSED`. The four are listed under
> [What was already found](#what-was-already-found); a fresh hang is a **new**
> defect and the ladder below is how to localise it.

Symptoms that land you here:

- a request that returns instantly works; the same request against a slower
  model/endpoint never returns
- `wget` from the same shell works while the real client is hung
- inserting a host-side proxy that answers instantly "fixes" it
- a client reports `TimedOut` / `Connection timed out` at a deadline it never
  configured

## Rule out a lost wakeup first — it cannot be one

The native stack has no wait that can miss a wake
(`../reference/subsystems/networking.md` § "The native data path"):

- `sys_epoll_pwait`, `sys_ppoll` and `sys_pselect6` re-drive
  `smoltcp_net::poll()` and re-check every fd at least every
  `BLOCKING_POLL_INTERVAL_US` = 10 ms, whether or not a `Waker` ever fires.
- `wait_until` (the blocking `recv`/`send`/`accept`/`connect` loop) polls up to
  64 times per round and then `blocking_relax()`es, which is
  `yield_now` + `idle_halt` — it returns ready-to-run, not parked on a wake.
- `KernelSocket::wakers` only shortens latency.

So a permanently blocked client needs one of: **the readiness oracle lies**, the
bytes never reach the smoltcp socket, or **the kernel returned a timeout nobody
asked for**. All four defects found in 2026-08 were in the first and third
categories. None was a lost wakeup.

## What was already found

Four defects, all fixed 2026-08-17. Knowing their shapes is most of the value
here, because a new hang is likely to rhyme with one of them.

| # | Defect | How it presented | Fix |
|---|---|---|---|
| 1 | Blocking TCP read capped at **30 s**, write at **5 s** | `ETIMEDOUT` at a deadline the client never set; a 35 s-delayed response died at 30069 ms, and a 40 s **mid-stream** idle died at 30125 ms | `socket_recv`/`socket_send` take the per-socket timeout, default `None` |
| 2 | `SO_RCVTIMEO`/`SO_SNDTIMEO` accepted and dropped; no `getsockopt` arm | a 2 s timeout fired at 30041 ms (defect 1's cap), and readback said the option was unset | real `struct timeval` plumbing, zero = forever, readback works |
| 3 | `EPOLLET` **write** edge never re-armed | a client that filled the 16 KB transmit buffer waited forever for `EPOLLOUT`; intermittent, because `epoll_pwait` flushes the buffer itself before it can observe `can_send()` go false | `epoll_on_fd_write_blocked`, called from `sendto`/`sendmsg`/`write` |
| 4 | A socket in **`SynSent`** reported read-closed | `EPOLLIN` + `EPOLLRDHUP` and `recv() == Ok(0)` on a connection that had never carried a byte; client parked forever **without sending its request** — ~1 run in 3 | `tcp_reached_established` guards both predicates |

The four were found in that order, and **the order matters as a warning**:
fixing 3 on its own left the 64 KiB POST still hanging roughly 1 run in 3, which
is easy to misread as "the fix didn't work" rather than "there is a second race
here". Two independent races with overlapping symptoms is the situation this
runbook exists for — do not stop at the first green run, and do not assume one
fix explains the whole symptom until the repeat test in step 5b is clean.

Defect 4 was the dominant one and it is the reason this symptom looked like it
was about *delay*. It is a race against the SYN window, so anything that shifts
timing — a bigger request body, a slower peer — changes how often it fires.
Defect 1 is why it looked like it was about the *first byte*: the 30 s cap
killed mid-stream reads just as readily.

Two lessons worth carrying into a new investigation:

- **`may_recv() == false` does not mean EOF.** smoltcp says that both before a
  connection is up and after the peer's FIN. Any new predicate that reads it
  needs `tcp_reached_established`.
- **An edge-triggered `last_ready` bit is only re-armed by an I/O syscall.**
  `epoll_pwait` refreshes the mask inside its own loop, so it cannot see a
  transition that happens and un-happens between passes. If you add a new
  readiness bit, add its reset hook at the same time.

## Setup

Host, in its own terminal:

```bash
scripts/net_delay_server.py --port 18080 --verbose
```

It serves `/health`, `/delay/<s>`, `/gap/<pre>/<gap>`, `/sse/<gap>/<n>`,
`/drip/<total>/<n>` and `/big/<mb>[/<s>]`. The guest reaches it at
**`http://10.0.2.2:18080`** — guest→host over SLIRP needs no `hostfwd` rule.

Build and ship the probes:

```bash
userspace/nettest/rust/build-musl.sh    # -> bootstrap/bin/nettest-{std,reqwest}
scripts/populate_disk.sh                # -> /bin in the image
```

Take the **host control** first, so you know the server and the probes are
sound before you blame the kernel:

```bash
cd userspace/nettest/rust/stdlib && cargo build --release --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)"
target/$(rustc -vV | grep '^host:' | cut -d' ' -f2)/release/nettest-std sweep http://127.0.0.1:18080
```

All rows must pass. If they do not, fix that before booting anything.

## Procedure

Boot the VM and SSH in
(`ssh -o StrictHostKeyChecking=no root@localhost -p 2222`), then run the ladder
in order. Each step narrows the stack by one layer.

### 1. Is it the blocking recv path?

```
nettest-std sweep http://10.0.2.2:18080
```

Expect one `[probe] SWEEP delay=<n>s OK first_byte_ms=…` line per delay in
`0,1,3,5,8,12,20,35`, then `SWEEP SUMMARY all 8 delays passed (max 35s)`.
`overhead_ms` should stay around 100 ms at every rung — that is SLIRP plus the
poll cadence, and it must NOT grow with the delay.

- **`threshold: last OK=20s, first FAIL=35s`** with `kind=TimedOut` is
  defect 1 come back: a blocking read capped at ~30 s. Check
  `socket_recv`'s `wait_until` timeout argument.
- **A threshold anywhere else** (e.g. last OK=3 s, first FAIL=5 s) is a new
  finding — record `stage=` and `kind=` from the `RESULT fail` line.

### 2. Does `SO_RCVTIMEO` do anything?

```
nettest-std rcvtimeo http://10.0.2.2:18080/delay/30 2
```

Expect `[probe] VERDICT SO_RCVTIMEO honoured`, with `readback=2000ms` and the
read failing at ~2000 ms. `NOT honoured — waited 30000ms for a 2s timeout` is
defects 1+2 together; `readback=NONE` alone means the `getsockopt` arm is gone.

### 3. Is it the readiness path rather than blocking recv?

```
nettest-std sweep http://10.0.2.2:18080 0,1,3,5,8,12,20,35 poll
```

Same ladder, but nonblocking sockets driven by `poll(2)` — `sys_ppoll`'s
readiness reporting, no `epoll`. Also watch the `connect redial errno=` line:
`0` or `EISCONN` is correct, anything else is a `connect_step` regression.

- step 1 fails, step 3 passes → the fault is in `wait_until` / blocking recv.
- step 1 passes, step 3 fails → the fault is in readiness reporting
  (`epoll_check_fd_readiness`, `socket_can_recv_tcp`).

### 4. Is it TLS?

```
nettest-std tls https://<a real https endpoint>/
```

Blocking rustls over a blocking `std::net::TcpStream` — the same rustls major
nca links, with no async runtime under it.

### 5. Is it nca's stack?

```
nettest-reqwest sweep http://10.0.2.2:18080
```

tokio + mio (`epoll_pwait` on `O_NONBLOCK` sockets) + hyper 1.x + reqwest 0.12
with `rustls-tls` — nca's dependency line verbatim. If steps 1–4 pass and this
fails, the fault is in `sys_epoll_pwait` or in how the reactor uses it.

Useful variations:

```
NETTEST_RT=current  nettest-reqwest sweep http://10.0.2.2:18080   # reactor shares the app thread
NETTEST_NEW_CLIENT=1 nettest-reqwest sweep http://10.0.2.2:18080   # no connection reuse
NETTEST_HTTP1=1      nettest-reqwest get https://…                 # split HTTP/2 off as its own axis
```

### 5b. Does a large request body survive the connect race?

```
for i in $(seq 1 12); do nettest-reqwest post http://10.0.2.2:18080/delay/0 64; done
```

This is the repro for defects 3 and 4, and it is the one that needs
**repetition** — both were races, so a single passing run proves nothing. All 12
must print `RESULT ok`. A hang here looks like the probe stopping after
`[probe] post body=65575 bytes` with nothing further.

When one hangs, the discriminator is the **host** log
(`net_delay_server.py --verbose`):

| Host log shows | Meaning |
|---|---|
| nothing at all for that run | the guest sent zero bytes on an ESTABLISHED socket — defect 4's shape (a connecting socket reported read-closed) |
| `REQUEST-LINE` then `body <n>/65575B` stalling partway | the guest stopped mid-body — defect 3's shape (the `EPOLLOUT` edge was never re-armed) |
| the full body and a response | the request completed; the hang is on the read side, go to step 6 |

Check `/proc/net/tcp` in the guest while it is hung — an `ESTABLISHED` entry to
port 18080 with no server-side log line is the defect-4 signature exactly.

### 6. Delayed first byte, or any long idle?

The archive doc could not tell these apart. This can:

```
nettest-std     gap http://10.0.2.2:18080 0 20
nettest-reqwest gap http://10.0.2.2:18080 0 20
```

The server sends a chunk immediately, idles 20 s, then sends a second chunk.

- `GAP FAIL … died before the FIRST byte` → delayed-first-byte class.
- `GAP FAIL … died MID-STREAM` → **any long idle** on an established
  connection, and the archive doc's framing is wrong.
- `GAP OK` with a 20 s inter-chunk gap → only the first byte is affected.

### 7. Throughput, not correctness

```
nettest-std raw http://10.0.2.2:18080/big/8
```

The device posts one 2 KB virtio RX buffer at a time and there is no RX
interrupt, so a big burst drains at roughly one frame per poll pass. A slow but
completing transfer here is expected and is not the bug you are chasing.

## Reading the result matrix

| `std raw` | `std poll` | `std tls` | `reqwest` | Diagnosis |
|---|---|---|---|---|
| OK | OK | OK | OK **but** step 5b hangs | A race, not a level bug — defect 3 or 4's territory. The host log in step 5b says which. This is the combination the 2026-08 investigation actually landed on, and a plain sweep does **not** catch it. |
| FAIL | FAIL | FAIL | FAIL | kernel-wide: `wait_until` and the readiness oracle share `socket_can_recv_tcp`, or the bytes never arrive. Go to `smoltcp_net::poll()` and the virtio RX path. |
| FAIL | OK | OK | OK | blocking recv only — `socket_recv`'s `wait_until` (most likely its 30 s cap). |
| OK | FAIL | — | FAIL | readiness reporting — `epoll_check_fd_readiness` / `socket_can_recv_tcp` / the `EPOLLET` `last_ready` bookkeeping. |
| OK | OK | FAIL | FAIL | TLS — but note both TLS paths share rustls, so suspect record framing over a slow socket, not the crypto. |
| OK | OK | OK | FAIL | `sys_epoll_pwait` or the tokio reactor's use of it. Re-run with `NETTEST_RT=current` and `NETTEST_HTTP1=1` to split the axis further. |
| OK | OK | OK | OK | and step 5b clean too → not reproduced. Widen: real SSE (`/sse/1/20`), an 8 MiB download (`/big/8`), or go back to nca with the kernel's `[TCP]`/`[epoll]` traces on. |

## Kernel-side traces

Set `SYSCALL_DEBUG_NET_ENABLED` (`src/config.rs`) and re-run the failing step.
The lines that matter:

- `[TCP] recvfrom fd=N got=…` / `err=…` — every socket read and its outcome.
- `[sock] read fd=N EAGAIN (drained)` — the `epoll_on_fd_drained` edge reset.
- `[epoll] pwait ret pid=… nready=… iters=… dur_us=…` — one line per
  `epoll_pwait` return; `iters` is how many 10 ms rounds it spun.
- `[epoll] pwait still waiting: … <n>us elapsed` — every ~5 s while parked.
- `[syscall] connect(fd=N, ip=A.B.C.D:port)` and its result.

An `epoll_pwait` that is genuinely stuck prints a growing `still waiting` line
with `nready=0`. That means the readiness oracle keeps saying "not ready" — go
read `socket_can_recv_tcp` and the smoltcp socket state, not the waker list.

## Verify

You have a usable result when all of these hold:

1. The host control run (`nettest-std sweep` against `127.0.0.1`) passes every
   row — the server and probes are sound.
2. The guest run produced a `SWEEP SUMMARY` line naming a specific threshold,
   or "all N delays passed".
3. For any failing row you have the `RESULT fail stage=… after_ms=… kind=…`
   line, so the failure is attributed to connect / send / first-byte / body.
4. `nettest-std` and `nettest-reqwest` were both run against the **same** base
   URL in the same boot, so the matrix above can be read.
5. `nettest-std gap` was run, so the delayed-first-byte vs any-long-idle
   question is answered rather than assumed.
6. Step 5b was run **at least 12 times**, because defects 3 and 4 were races
   that a single run passes by luck.
7. If you changed the kernel to get here, the boot log still shows SSH working
   and the suite still green. A socket fd released with a bare
   `remove_socket()` instead of `sys_close` double-drops on deferred reap and
   silently destroys whatever owns the recycled slot — that is how this very
   investigation broke sshd for four boots
   (`../reference/subsystems/networking.md` -> "Socket lifetime").

For reference, these are the numbers a healthy kernel produced on 2026-08-17,
against `net_delay_server.py` on the host at `MEMORY=2048`:

| Measurement | Result |
|---|---|
| `nettest-std sweep` (blocking) | all 8 delays to 35 s pass, overhead ~60–140 ms |
| `nettest-std sweep … poll` | all 8 pass |
| `nettest-reqwest sweep` | all 8 pass |
| `nettest-std rcvtimeo …/delay/40 2` | honoured, fired at 2009 ms |
| `gap 0 40` (both probes) | OK, second chunk at ~40.1 s |
| `nettest-reqwest post … 64` ×12 | 12/12 OK |
| `nettest-reqwest post …/delay/3 128` | OK |
| `nettest-std raw …/big/8` (8 MiB) | OK, 1025 chunks in 917 ms (~9 MB/s) |
| `nettest-reqwest stream …/sse/1/5` | OK, 5 events at ~1 s spacing |
| boot suite | 281 PASSED, 0 FAILED |

## Background

- [`../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md)
  — the original investigation, its ruled-out list, and the lost-wakeup
  hypothesis this runbook retires.
- [`../archive/NCA_MISSING_SYSCALLS.md`](../archive/NCA_MISSING_SYSCALLS.md) §2b
  — the compact version of the same symptom.
- [`../reference/subsystems/networking.md`](../reference/subsystems/networking.md)
  § "The native data path" — the audit: poll drivers, RX buffering, readiness
  predicates, and the full divergence table.
- [`debug-async-subprocess-hang.md`](debug-async-subprocess-hang.md) — the same
  edge-triggered class on **pipes** rather than sockets: defect 3's read-side
  twin, found 2026-08-17 when `epoll_on_fd_drained` turned out to be wired into
  the socket paths only. Read it if a *child process* hangs rather than a socket.
- [`debug-network.md`](debug-network.md) — general native-stack debugging.
- [`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) — the lost-wakeup
  hunt whose shape the archive doc borrowed. Kept as a cross-reference for the
  method, not because the socket path has the same defect.
