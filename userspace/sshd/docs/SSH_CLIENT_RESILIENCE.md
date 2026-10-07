# SSH client resilience to lossy connections

## 1. Symptom

On a lossy link (flaky wifi, dying NAT state), the `ssh` client had four
rough edges, all variants of "the connection hiccupped and the client made it
worse or invisible":

1. A single dropped SYN at connect time ended the session before it started.
2. Any stall during the handshake (version exchange, KEX, auth) blocked
   forever inside the kernel's `recv()` with no message — indistinguishable
   from a hung terminal.
3. An idle session gave the network *zero* traffic, so NAT/firewall state
   timed out silently and the first sign of death was a frozen terminal — or
   nothing at all, forever.
4. One `EINTR`/`ETIMEDOUT` from a read syscall killed the whole session.

## 2. Fixes (`userspace/sshd/src/client/protocol.rs`)

All timing is based on `libakuma::uptime()` (monotonic µs), never `time()`,
so deadlines can't be skewed by RTC changes. Everything degrades gracefully:
a healthy connection sees no behavior change at all.

### Connect retries

`connect_with_retry`: up to `CONNECT_ATTEMPTS` (3) `TcpStream::connect`s
with a linear 500 ms backoff between them, warning on stderr per failed
attempt. The kernel already SYN-retransmits inside a single blocking
`connect()`; this layers whole-attempt retries on top for the
"attempt itself failed fast" case.

### Handshake deadlines

The socket is switched to non-blocking immediately after connect (the pump
already ran that way; now the handshake does too, via
`wait_for_handshake_io`):

- `HANDSHAKE_IDLE_TIMEOUT_MS` (30 s): no *bytes at all* from the peer for
  this long during the version line / KEX / auth →
  "connection stalled during handshake". Reset by every received byte, so a
  genuinely slow but live server keeps making progress.
- `HANDSHAKE_TOTAL_TIMEOUT_MS` (120 s): absolute ceiling on the whole
  handshake → "handshake timed out".

`read_version_line` was rewritten to consume from `input_buffer` under the
same deadline discipline, which also fixes a latent pipelining wart: a server
that sends banner + version line + KEXINIT in one TCP write previously had
its already-buffered bytes at risk of being read twice or lost; now whatever
follows the version line stays in `input_buffer` for `recv_packet`.

### Client keepalive (the main event)

OpenSSH's client sends `keepalive@openssh.com` global requests on
`ServerAliveInterval` and declares the connection dead after
`ServerAliveCountMax` unanswered probes. This client now does the same,
self-timed:

- After `DEFAULT_ALIVE_INTERVAL_MS` (15 s) of inbound silence, send
  `keepalive@openssh.com` with `want_reply = 1`. Any inbound packet —
  the `SSH_MSG_REQUEST_FAILURE` a non-OpenSSH server must send back
  included — resets the liveness counters, so the loop needs no reply
  bookkeeping beyond "bytes arrived".
- After `DEFAULT_ALIVE_COUNT_MAX` (3) consecutive unanswered probes, exit
  with `Timeout, server <host> not responding.` — OpenSSH's exact message —
  instead of hanging.
- Runtime-tunable via `SSH_ALIVE_INTERVAL` (seconds, `0` = off, matching
  OpenSSH's convention) and `SSH_ALIVE_COUNT_MAX`.

Side effect worth having: even when the peer is perfectly healthy, the
probes keep traffic flowing on an idle session, so NAT entries and
power-saving wifi links don't silently reap it.

### Transient read errors

`Interrupted` and `TimedOut` from the pump's socket read no longer end the
session. If the peer is genuinely gone, the keepalive check above bounds the
detection time; if it was one bad syscall, the user never notices.

### Auto-reconnect (on by default)

`run` wraps the whole session (`run_once`: resolve → connect → handshake →
channel → pump) in a retry loop. Any error — connect refused mid-flap,
handshake deadline, keepalive-dead, reset mid-session — starts over from a
clean slate: new TCP connection, new KEX, re-auth, new channel. A clean
remote exit (`Ok`) never reconnects.

- `SSH_AUTO_RECONNECT` — default `1` (on); `0` restores the old
  fail-on-first-error behavior.
- `SSH_RECONNECT_ATTEMPTS` — total attempts including the initial
  connection, default 10.
- Delay between attempts is linear from a 2 s base (`attempt × 2 s`),
  capped at 30 s: `client_resilience::reconnect_delay_ms`.

Semantics worth knowing: the *session* does not survive server-side. A
reconnected interactive shell gets a fresh login, and a reconnected `exec`
runs the command again from the top — this is "get a working prompt back
fast on a flapping link", not Mosh-style session resumption. Terminal state
is restored between attempts (raw mode off, stdin back to blocking), so the
reconnect messages print normally and a TOFU prompt on a subsequent attempt
still reads cooked stdin.

### Host-tested policy logic

The decision math lives in the host-testable **lib** target
(`src/client_resilience.rs`, `sshd::client_resilience`), not in the binary:
`Keepalive` (probe scheduling, liveness reset, dead-declaration) and the
reconnect helpers (`reconnect_delay_ms`, `should_retry`) are pure functions
of their arguments — the caller passes `now_ms()`, so tests are
deterministic. The pump in `protocol.rs` is a thin driver over them.
10 unit tests cover the interval boundary, disabled keepalive, the
`count_max + 1` death rule, liveness reset, backoff linearity/cap, and the
attempt-count semantics.

```bash
cd userspace && cargo test -p sshd --lib --no-default-features \
    --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

## 3. Non-goals

No session *resumption* — the reconnect starts a new session, it doesn't
pretend the old one survived (see above). No server-side keepalive config
(`TCPKeepAlive` analog); the application-level probe already covers it.

## 4. Verification

- `cargo build --release -p sshd` (aarch64-unknown-none, `-Zbuild-std`)
  clean; both `ssh` and `sshd` binaries link.
- `cargo test -p sshd --lib --no-default-features --target <host>`: 43
  tests pass — the 10 new `client_resilience` tests plus the pre-existing
  `client_wire`/`wire` framing tests.
- Not live-tested against an actual dropping link; each failure path has a
  clear user-visible message so a false positive (declared-dead-but-alive)
  would be obvious and cheap to tune via the env vars.

## Background

- `SSH_KEEPALIVE_TIMEOUT_FIX.md` — the server-side counterpart: `sshd` now
  *answers* `want_reply` global requests; this change makes the client
  *send* them.
- `SSH_CLIENT.md` — client scope/usage.
