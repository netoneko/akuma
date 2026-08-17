# Socket read hangs forever when the response's first byte is delayed

**Date:** 2026-08-17. **Found:** verifying `nca` (upstream
`native-cli-ai`, host-built for `aarch64-unknown-linux-musl`) against host
Ollama from the devbox-smoltcp guest. **Status: FIXED 2026-08-17** (originally filed OPEN with a
lost-wakeup hypothesis; see the Resolution block below — it was four separate
defects and none of them was a lost wakeup).
Cross-refs: `NCA_MISSING_SYSCALLS.md` §2b (compact version), DEVBOX_ISSUES
Issue 17, `docs/runbooks/debug-network.md`.

> **Update 2026-08-17 — carried forward to
> `docs/runbooks/debug-delayed-first-byte.md`.** An akuma-net audit
> (`docs/reference/subsystems/networking.md` § "The native data path")
> **retires the "Working hypothesis" below**: no wait in the native stack
> sleeps on a wake it can miss. `sys_epoll_pwait`/`sys_ppoll`/`sys_pselect6`
> re-drive `smoltcp_net::poll()` and re-check every fd at least every 10 ms
> regardless of wakers, and `wait_until` re-polls after every
> `blocking_relax()` (`yield_now` + `idle_halt`, which returns ready-to-run,
> not parked). `KernelSocket::wakers` is a latency optimisation.
>
> Three undeclared kernel timeouts the audit surfaced are better suspects, and
> the second is the closest number in the tree to the ~5 s threshold observed
> below: a blocking TCP read is capped at **30 s** (`ETIMEDOUT`), a blocking
> TCP write at **5 s**, and `SO_RCVTIMEO`/`SO_SNDTIMEO` are accepted and
> silently dropped.
>
> The "Minimal repro to build next" section at the end of this doc is now
> **built**: `scripts/net_delay_server.py` (host) plus `nettest-std` and
> `nettest-reqwest` (`userspace/nettest/rust/`), which sweep the delay and
> answer the delayed-first-byte-vs-any-long-idle question this doc left open.
> Everything below is preserved verbatim as the original investigation.

> **Resolution 2026-08-17 — ROOT-CAUSED and FIXED (four defects).** Measured in
> a VM with the probes above; each fix carries a regression test.
>
> 1. **Blocking TCP read capped at 30 s** (`socket_recv`'s
>    `wait_until(..., Some(30_000_000))`). A response delayed 35 s died at
>    30069 ms with `ETIMEDOUT`. Blocking write had the same shape at 5 s.
>    Both are now `None` (block forever) unless the caller sets a timeout.
> 2. **`SO_RCVTIMEO`/`SO_SNDTIMEO` accepted and silently dropped**, with no
>    `getsockopt` arm at all. A 2 s `SO_RCVTIMEO` fired at 30041 ms — the
>    kernel's own cap, not the caller's. Now implemented as a `struct timeval`
>    with POSIX zero-means-forever and a working readback.
> 3. **The `EPOLLET` write edge was never re-armed.** `epoll_on_fd_drained`
>    existed for `EPOLLIN` and had no `EPOLLOUT` counterpart, so a client that
>    filled the 16 KB transmit buffer and then waited for `EPOLLOUT` could wait
>    forever — `epoll_pwait` drives `smoltcp_net::poll()` at the top of its own
>    loop and so usually flushed the buffer before ever observing `can_send()`
>    go false. Added `epoll_on_fd_write_blocked`, called from `sendto`,
>    `sendmsg` and `write` on every short write and every `EAGAIN`.
> 4. **A socket still in `SynSent` was reported read-closed** — and this was the
>    dominant defect. smoltcp answers `is_active() == true` and
>    `may_recv() == false` mid-handshake, which is the same pair a peer's FIN
>    produces, so the readiness oracle raised `EPOLLIN` + `EPOLLRDHUP` and a
>    non-blocking `recv` returned `Ok(0)` on a connection that had never carried
>    a byte. A client that polled inside that one-round-trip window concluded
>    the connection was dead and parked forever **without sending its
>    request** — reproduced as `nettest-reqwest post <url> 64` hanging ~1 run in
>    3 with the socket ESTABLISHED and zero bytes at the server. Fixed with
>    `tcp_reached_established` / `tcp_recv_ready`.
>
> **The framing in this doc was wrong in two ways.** The 30 s cap killed
> *mid-stream* reads too (a 40 s idle after a successful first chunk died at
> 30125 ms), so it was never specific to the first byte. And defect 4 is a race
> against the SYN window, not against prefill time — the correlation with
> gemma's ~2900-token prompt was request *size* changing the timing, not the
> model being slow. Nothing here was a lost wakeup.
>
> After the fixes: every delay to 35 s passes on all three probe stacks, a 40 s
> mid-stream idle completes, `SO_RCVTIMEO` fires at 2009 ms for a 2 s timeout,
> 64 KiB POST is 12/12 clean, and the boot suite is 281 PASSED / 0 FAILED.
> Procedure and result matrix: `docs/runbooks/debug-delayed-first-byte.md`.

## Symptom

A guest TCP client connects to a host service over SLIRP, sends its request,
and then **blocks forever in read** when the server takes more than ~5
seconds before its **first response byte**. Identical requests whose answers
start arriving within ~1 s stream perfectly — including full SSE streaming.

Concretely, with nca wired to Ollama via the Custom provider:

- `model = "qwen3.5:0.8b"` — instant prefill → `2+2 → 4`, `3+3 → 6`,
  whole session lifecycle completes (`Session ended (Completed)`), twice.
- `model = "gemma4:e4b"` — a ~2900-token system prompt means a multi-second
  silent prefill window → `Connected to gemma4:e4b`, prompt sent, then the
  process sits in read until killed. 100% reproducible over several runs.
- **The identical gemma request succeeds end-to-end when pointed at a
  host-side Python logging proxy** (`10.0.2.2:18082` → `127.0.0.1:11434`)
  that answers the initial models-list probe instantly and then pipes chunks
  as they arrive. Same guest, same binary, same model, same SLIRP path — the
  proxy log shows the complete SSE body delivered to the guest.

The proxy is the discriminator: it changes *timing*, not topology.

## Ruled out

- **SLIRP / host reachability:** `wget` GET and POST to
  `http://10.0.2.2:11434/v1/...` work from the guest at all times, including
  while an nca run is hung. And the proxy run traverses the same SLIRP.
- **Ollama-side stalls:** the model is GPU-resident and warm
  (`ollama ps`: gemma4:e4b, 100% GPU, no expiry). The direct
  `/v1/chat/completions` POST from the guest via `wget` returns chunks.
- **Keep-alive connection reuse:** a std guest client doing two sequential
  `GET /v1/models` over **one** connection gets both responses (so the
  request/response loop itself is fine when answers are instant).
- **Non-blocking I/O specifically:** a std probe (`connect` blocking, then
  `set_nonblocking(true)`, write, poll-read) against a host listener
  delivers and receives fine — again with an instant reply.
- **nca/reqwest bugs:** the same binary completes round trips through the
  proxy, which preserves method/headers/streaming semantics.

## Working hypothesis

When a socket has been idle in a blocked read for a long window and bytes
then arrive, the wake of the blocked reader is **lost** — the reader never
re-polls and sleeps forever. Every passing test above got first bytes within
~1 s; every failing test had a >5 s silent window (gemma prefill ≈ 10 s;
cold 10 GB model load ≈ minutes). Sub-second-chunk streaming (the proxy)
stays healthy, consistent with a wakeup being missed only for a reader
parked "long enough" — e.g. a wake routed to a stale wait entry, a timeout
path that unregisters the waiter, or an rx-notification consumed by an
earlier poll.

Alternative (less likely): the host side of SLIRP closes/re-uses the
forwarded connection during the silent window and the guest never notices —
but the direct `wget` POST during the hang argues the forwarding stays
alive.

## Suspect surface (kernel)

- `crates/akuma-net/src/socket.rs` — rx notification / wait queue
  bookkeeping for blocked reads.
- `src/syscall/net.rs` `sys_recvfrom`/`sys_read` on sockets and their
  poll-integration (`sys_poll`/`sys_ppoll` wake registration).
- `src/syscall/poll.rs` — pollfd wake registration vs timeout teardown.
- Prior art in the same shape: the futex lost-wakeup hunt
  (`docs/runbooks/debug-futex-lost-wakeup.md` §4a — "published WAITING
  before re-reading the sticky flag" is exactly the class of race to look
  for here, on the socket rx path instead of the futex path).

## Minimal repro to build next

Guest side (std only, blocking):

```rust
// connect to 10.0.2.2:<port>, write "GET /delay HTTP/1.1\r\n\r\n",
// read with a long timeout — print elapsed time of first byte.
```

Host side: `python3` one-liner that accepts, reads the request, `sleep`s
parameterized seconds (0.5 / 2 / 5 / 10 / 30), then sends a response. Sweep
the delay to find the hang threshold; if 0.5 s works and 10 s hangs, also
test a mid-stream gap (fast first chunk, 10 s pause, second chunk) to
distinguish "first byte" from "any long idle".

If the threshold turns out to be tied to the guest's *poll* timeout rather
than the arrival timing, the bug is in wake-registration teardown on poll
timeout — check that a timed-out poll leaves the socket's waiter list in a
state that a later rx notification can still enter.
