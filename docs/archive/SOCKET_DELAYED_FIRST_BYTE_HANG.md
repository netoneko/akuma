# Socket read hangs forever when the response's first byte is delayed

**Date:** 2026-08-17. **Found:** verifying `nca` (upstream
`native-cli-ai`, host-built for `aarch64-unknown-linux-musl`) against host
Ollama from the devbox-smoltcp guest. **Status: OPEN, kernel suspicion —
lost wakeup of a blocked socket reader after a long idle window.**
Cross-refs: `NCA_MISSING_SYSCALLS.md` §2b (compact version), DEVBOX_ISSUES
Issue 17, `docs/runbooks/debug-network.md`.

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
