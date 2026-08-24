# `httpd` stops answering while still running (2026-08-25)

**Status: OPEN, observed 2-3 times, NOT reliably reproduced and NOT diagnosed.**
The failing stage is not established — see §2, which is deliberately explicit
about what the evidence does and does not show. Sibling of
[`NGINX_LOST_WAKEUP.md`](NGINX_LOST_WAKEUP.md); §5 is why they are probably
related.

> **The one-line answer.** `userspace/httpd` stops answering HTTP requests while
> the **process is still alive**, still in `ps`, with its log still reading
> `httpd: Listening for connections...` and **no error line**. Every request
> then returns `000` (connection failure) from **both** the host and in-guest.
> It does not crash, does not exit, and logs nothing.

---

## 1. What was observed

Three episodes, same session, same kernel (trace-gated `devbox-smoltcp`,
`SMP=4`, `devbox.img`):

| # | server | client | outcome |
|---|---|---|---|
| A | `/bin/httpd 8090`, verbose | in-guest `curl` | log shows **4** accepted connections with `GET /` parsed; client saw `000` on all of them |
| B | `/bin/httpd 4444`, `HTTPD_QUIET=1` | host `ab` through hostfwd | one **fully successful** run — 200/200 complete, 0 failed, 1,930 rps — then every subsequent request `000`, from host *and* in-guest |
| C | `/bin/httpd 8090`, quiet | in-guest `ab` | **3 x 200 requests all succeeded** (909/828/914 rps), server still answering `200` afterwards |

So it is **intermittent**: C is a clean 600-request run on the same binary and
kernel that hung in A and B.

State at the point of failure (checked in B):

```
host->akuma httpd :4444 = 000
httpd still in ps?: 2          # process alive
httpd log tail: httpd: Starting HTTP server on port 4444
                httpd: Listening for connections...      # no error, no new lines
in-guest 4444: 000             # not a NIC/SLIRP problem — fails locally too
```

## 2. What the evidence does NOT establish

**Which stage hangs is unknown**, and the two episodes point at *different*
stages:

- In **A** the log proves httpd got as far as `accept` returning **and** the
  request being read (`connection from ...` is printed after accept, `GET /`
  after parsing) — yet the client got `000`. That is consistent with a failure
  to **write the response**, or with the connection dying after the read.
- In **B** the instance was `HTTPD_QUIET=1`, so those per-request lines were
  suppressed. **Nothing in that log can say whether it was still accepting.**
  It is therefore *not* established that `accept` is where it stops.

An earlier draft of this note claimed "the accept loop is hung". That was an
overstatement from B alone; A actively contradicts it. **Reproduce with
verbose logging on before believing any stage-level claim.**

What *is* solid: the process is alive, logs no error, and serves nothing, on a
listener that worked moments earlier.

## 3. Why "no error logged" is informative

`httpd`'s accept loop (`userspace/httpd/src/main.rs`) prints on the error path:

```rust
Err(e) => {
    st.mark(Phase::Accept);
    if e.kind != libakuma::net::ErrorKind::WouldBlock {
        print("httpd: Accept error: ");      // ungated
        print(&format!("{:?}\n", e));
    }
    libakuma::sleep_ms(1);
}
```

That print is **not** behind `HTTPD_QUIET`, so a persistent accept error would
spam the log every millisecond in any configuration. The log is silent.
Therefore `accept` is **not returning an error** — it is either blocked inside
the kernel and never returning, or returning connections that then fail
downstream.

`libakuma`'s `TcpListener::accept` (`userspace/libakuma/src/net.rs:150`) loops
on `EAGAIN` and otherwise returns the error, so a busy-spin on `EAGAIN` is also
possible and would look identical from outside (alive, silent, serving
nothing) — though it would burn CPU, which was not checked at the time.
**Check guest CPU next time**: that single observation separates
"blocked in the kernel" from "spinning on EAGAIN".

## 4. There is no epoll anywhere in this path

Verified by grep over both crates: `userspace/httpd/src/*.rs` and
`userspace/libakuma/src/` contain **no `epoll`, `poll`, `ppoll` or `pselect`**.
`httpd` is a single-threaded blocking `accept` / `read` / `write` loop, and
`libakuma` calls the raw `accept` / `recv` / `send` syscalls directly. So this
hang is on `akuma_net::socket::wait_until` — the socket wait family — and has
nothing to do with the epoll path.

## 5. Why this and the nginx bug are probably one story

[`NGINX_LOST_WAKEUP.md`](NGINX_LOST_WAKEUP.md) describes nginx losing readiness
wakes on `sys_epoll_pwait` and being rescued by the 10 ms `backstop_us`. This
note describes a blocking `wait_until` waiter that stops being served with no
error.

**Those are the two wait families** that `akuma-net-yarn` was extracted to make
comparable, and which
[`../reference/subsystems/syscalls/poll.md`](../reference/subsystems/syscalls/poll.md)
documents as differing in six policy fields — including `backstop_us` (10 ms vs
3 ms) and `epoch_guard` (off vs on). One symptom per family, both looking like
a wake that does not arrive, is suggestive enough to record — but it is a
**hunch, not a finding**. Nobody has shown a common cause, and the difference in
symptom (nginx recovers every 10 ms, httpd never recovers) may well mean they
are unrelated: the epoll family has a backstop that rescues it, and
`wait_until`'s 3 ms backstop should rescue it too, which it evidently does not.
**That asymmetry is the most interesting single question here** — if
`wait_until` also has a backstop, why does httpd never come back?

## 6. How to attack it

1. **Reproduce with logging ON** (`/bin/httpd <port>` with no `HTTPD_QUIET`), so
   the `connection from` / `GET` lines can place the failure. Run the load from
   the **host** through hostfwd, which is how episode B was provoked.
2. **Check guest CPU at the moment of failure** — §3: spinning vs blocked.
3. **Dump kernel-side thread state.** `dump_thread_resume_points()` cracked a
   structurally similar hang before
   ([`PHASE7E_SIGPIPE_DEFERRED_DROP`-class work](../runbooks/debug-smp.md)); the
   question to answer is whether httpd's thread is parked in `wait_until` with a
   deadline that never fires, or runnable and never scheduled.
4. **Check the listener socket still exists** — `MAX_SOCKETS` exhaustion and
   socket-table leaks have produced "listener silently stops working" before in
   this tree (see the `recreate_listener_with_retry` row in
   [`../runbooks/debug-network.md`](../runbooks/debug-network.md)). A leaked
   socket per connection would explain why it survives 600 requests once and
   dies after 200 another time.

## 7. Harness note

Do not benchmark this by killing the server between arms: a `pkill` of the
target leaves the in-guest `ab`/`curl` client **hung forever** on a socket that
will never complete, and those accumulate and contaminate later measurements
(`pkill -f /usr/bin/ab` between arms; check `ps`). Validate every HTTP run with
`%{http_code}` or `ab`'s `Complete/Failed requests` — a dead listener otherwise
measures as extremely fast, which is exactly how a phantom "6.8x speedup" was
manufactured earlier in this session
([`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) §9d).

## Background

- [`NGINX_LOST_WAKEUP.md`](NGINX_LOST_WAKEUP.md) — the epoll-family sibling.
- [`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) §9 — the
  measurement rules this session earned, and why HTTP numbers from it are not
  quotable.
- [`../runbooks/debug-network.md`](../runbooks/debug-network.md) — symptom
  table; this bug and the hung-client harness trap are rows there.
- [`../reference/subsystems/syscalls/poll.md`](../reference/subsystems/syscalls/poll.md)
  § "The wait loop is one machine" — the six fields separating the two families.
