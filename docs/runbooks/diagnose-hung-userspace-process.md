# Is this devbox process actually hung, or just slow?

Use this as the first triage step for "is X hung" on any devbox userspace
process reachable over SSH, **before** reaching for kernel tracing
(`SYSCALL_DEBUG_*`), gdb, or one of the deep network-readiness runbooks below.
It answers the yes/no question in under a minute using two signals that are
each ambiguous alone but conclusive together.

`ps aux` and `/proc/<pid>/status` are **not** useful for this: a thread
blocked forever inside a syscall still reports `State: R (running)`, and
`TIME 0:00` only rules out a hot spin loop, not a blocked read. Do not use
either as evidence of health.

## Steps

1. Sample `/proc/net/tcp` two or three times, a few seconds apart:

   ```bash
   ssh -o StrictHostKeyChecking=no -p 2222 root@localhost 'cat /proc/net/tcp'
   ```

   A connection sitting in `CLOSE_WAIT` to the **same** remote `ip:port`
   across every sample means the peer already sent FIN — the kernel has seen
   the close, but the app hasn't reacted (no `close()` sent back). By itself
   this is only a lead: idle connection pools sit on stale keep-alives all the
   time, and the `CLOSE_WAIT` socket may not even be the one the app is
   currently blocked on.

2. Check the process's own structured log, if it writes one. `nca` logs to
   `/.local/share/ncacli/workspaces/*/sessions/session-*.events.jsonl`:

   ```bash
   ssh -p 2222 root@localhost \
     'tail -c 2000 /.local/share/ncacli/workspaces/*/sessions/session-*.events.jsonl; date'
   ```

   Compare the timestamp of the last event against the guest's own `date`. A
   multi-minute gap while the app claims to be busy (nca: `BusyStateChanged
   thinking` right after a `Checkpoint provider_request`) is the signal.

3. Correlate. Neither signal alone is proof — an idle pooled connection in
   `CLOSE_WAIT` is normal, and an app can legitimately be slow. **Both
   together** — a `CLOSE_WAIT` that doesn't clear, timed against an app log
   that went silent mid-operation — is what confirms a real hang rather than
   a slow response.

## If it's confirmed hung

Don't stop here — this runbook only answers *whether*, not *why*. Two
existing runbooks cover the network-readiness defect classes that have
produced this exact symptom before:

- [`debug-delayed-first-byte.md`](debug-delayed-first-byte.md) — a client
  that stalls against a slow or closing peer; has the `nettest-std` /
  `nettest-reqwest` ladder and the result matrix that localises the fault to
  a specific layer (blocking recv, readiness reporting, TLS, or the tokio
  reactor).
- [`debug-async-subprocess-hang.md`](debug-async-subprocess-hang.md) — same
  edge-triggered readiness class, on pipes rather than sockets (a spawned
  child that exits fine but the parent never notices).

If neither's known-defect table matches, the ladder in
`debug-delayed-first-byte.md` step 5b onward is the right next tool: it is
built to reproduce exactly "the peer closes a connection out from under the
client" and tell you which layer swallowed the transition.

## Verify

You have a usable answer when you can state, with evidence, one of:

- **Not hung** — no `CLOSE_WAIT` stuck across samples, and the app log has
  recent events. Stop here.
- **Hung, and here is the socket** — a `CLOSE_WAIT` entry unchanged across
  samples, an app log gap starting near when that connection would have
  transitioned, and (if you have one) the fd/port correlation confirming it's
  the request actually in flight rather than an idle pooled connection.
- **Hung, cause unconfirmed** — the correlation isn't there yet (e.g.
  `/proc/net/tcp` has no inode column to match against a specific fd), so the
  next step is the reproduction ladder above, not a guess.

## Background

Found live 2026-08-22 diagnosing a real `nca` hang: a provider HTTPS
connection to the model API sat in `CLOSE_WAIT` unchanged for 8+ seconds
while nca's own event log had been silent for 3.5+ minutes mid a
`provider_request` turn. On inspection, the kernel's `CLOSE_WAIT` → `EPOLLIN`
→ `read() == 0` (EOF) chain — `epoll_check_fd_readiness`,
`socket_can_recv_tcp` (which already routes through `tcp_recv_ready`, see
[`debug-delayed-first-byte.md`](debug-delayed-first-byte.md) defect 4), and
`sys_recvfrom`'s unconditional `epoll_on_fd_drained` call on every successful
read including `Ok(0)` — reads correctly for a socket that reached
`Established` before `CLOSE_WAIT`. No defect was confirmed in that path by
code inspection alone; the `CLOSE_WAIT` socket observed may not have been the
one the stuck request was using. **Not yet root-caused** — this runbook
records the triage step that got that far, not the fix.
