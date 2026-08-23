# `Timeout, server localhost not responding` — unanswered global requests

## 1. Symptom

An interactive `ssh -p 2222 root@localhost` session — otherwise healthy,
actively idle rather than stuck on a stalled transfer — would eventually get
dropped by the **client** with:

```
Timeout, server localhost not responding.
```

This is not the same symptom as the other message with an identical string
in `docs/README.md`'s triage matrix: that one fires mid-transfer at exactly
1 MiB of piped data (`archive/SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md`, fixed
2026-08-13). This one fires on a session with **no transfer in flight at
all** — just sitting connected — after some multiple of the client's
`ServerAliveInterval`.

## 2. Root cause

`Timeout, server X not responding` is OpenSSH's own client-side message for
its keepalive mechanism: on an interval (`ServerAliveInterval`, off by
default but commonly set — e.g. macOS's system `ssh_config`), the client
sends `SSH_MSG_GLOBAL_REQUEST` "`keepalive@openssh.com`" with `want_reply =
true`. Per RFC 4254 §4, an implementation that doesn't recognize the request
name must reply `SSH_MSG_REQUEST_FAILURE` anyway — the client doesn't care
about the content of the reply, only that *something* answers, which is what
resets its liveness timer. After `ServerAliveCountMax` (default 3)
consecutive unanswered probes, it gives up with this exact message.

Akuma's `sshd` implements zero global requests, and — the actual bug —
never replies to one either. `handle_message`'s dispatch
(`userspace/sshd/src/protocol.rs`) matched every `SSH_MSG_*` type it
understood and fell through everything else, `keepalive@openssh.com`
included, into a bare `_ => {}`: parsed, silently discarded, no reply, ever.

## 3. Fix

`userspace/sshd/src/protocol.rs`:

```rust
const SSH_MSG_GLOBAL_REQUEST: u8 = 80;
const SSH_MSG_REQUEST_FAILURE: u8 = 82;
```

and a new arm in `handle_message`'s dispatch: read the request name (unused,
Akuma implements none) and the `want_reply` boolean that follows it: if set,
send back a bare `SSH_MSG_REQUEST_FAILURE` (no payload — the message code is
the entire packet). This is exactly what OpenSSH's own `sshd` does for
`keepalive@openssh.com`, since it doesn't implement that request either —
the point was never to implement keepalive semantics, just to answer.

## 4. Verification

`cargo check -p sshd --bin sshd` and the full kernel `cargo build --release`
both clean. Not yet re-tested against a live `ServerAliveInterval` timeout on
a real client (that requires waiting out `ServerAliveInterval *
ServerAliveCountMax` seconds of idle connection, which wasn't done as part
of this session) — the fix is a straightforward RFC 4254 §4 compliance gap
with an obvious, narrow root cause (traced directly to the `_ => {}`
catch-all silently swallowing a `want_reply` request), not a behavior
inferred from a symptom.

## Background

- `docs/README.md` symptom matrix — the *other* `Timeout, server localhost
  not responding` row (`archive/SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md`) is a
  different bug with the same client-side message; a session that drops
  after actually transferring ~1 MiB has that cause instead, not this one.
- `EXIT_STATUS_FIX.md`, `SECURITY_IMPROVEMENTS.md` — other `handle_message`
  dispatch gaps in the same file, same lesson: an unhandled message type
  that silently no-ops instead of returning the RFC-mandated failure/error
  response is invisible until a real client's own protocol-level bookkeeping
  (an exit status placeholder, a keepalive timer) notices nothing ever came
  back.
