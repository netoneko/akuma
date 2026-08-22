# `ssh` client vs a real server: unhandled interleaved messages

## 1. Symptom

The `ssh` client (`userspace/sshd/src/client/`) worked fine against Akuma's own
`sshd` but failed immediately against a real-world server:

```
$ ssh -p 22 tester@10.0.2.2
[ssh] connecting to 10.0.2.2:22...
[ssh] no /root/.ssh/id_ed25519; using sshd's host key (/etc/sshd/id_ed25519) as identity
ssh: expected CHANNEL_OPEN_CONFIRMATION, got message type 80
```

or, once that was fixed, one step later:

```
ssh: expected a reply to 'exec', got message type 93
```

Neither is a crash — the client returns a clean `ClientError` and exits 255 —
but from the outside it reads like one: the connection dies right around the
point it's using the identity key, with no useful explanation of what
actually went wrong.

## 2. Root cause

Both call sites read exactly one packet and pattern-matched it against the
one reply they expected, treating anything else as fatal:

```rust
let (msg_type, payload) = conn.recv_packet()?;
match msg_type {
    SSH_MSG_CHANNEL_OPEN_CONFIRMATION => { /* ... */ }
    SSH_MSG_CHANNEL_OPEN_FAILURE => { /* ... */ }
    other => return Err(/* "expected ..., got message type {other}" */),
}
```

That works only against a server that never sends anything else in between —
true of Akuma's own `sshd`, false of real ones. Two SSH-2 message types are
legitimately allowed to show up first:

- **`SSH_MSG_GLOBAL_REQUEST` (80)** — OpenSSH sends
  `hostkeys-00@openssh.com` (a "here are my other host keys" notice)
  immediately after auth succeeds, before it gets around to replying to the
  client's `CHANNEL_OPEN`. RFC 4254 permits global requests at any time once
  the connection service is running; nothing says the server has to finish
  answering an in-flight channel/request message first.
- **`SSH_MSG_CHANNEL_WINDOW_ADJUST` (93)** — a server can grow the channel's
  send window before it replies to a channel request (`pty-req`, `exec`,
  `shell`), since window credit and the request/reply are independent pieces
  of per-channel state.

The client's own interactive loop (`pump`, further down in the same file)
already handles both correctly — this was a gap specific to the two
handshake-phase call sites that predate the interactive loop's message
handling, not a case the client had never considered.

## 3. Fix

`userspace/sshd/src/client/protocol.rs`:

1. **`CHANNEL_OPEN` wait** (`run()`, right after sending `SSH_MSG_CHANNEL_OPEN`):
   wrapped the single `recv_packet()` in a loop that swallows
   `SSH_MSG_GLOBAL_REQUEST` (replying `SSH_MSG_REQUEST_FAILURE` if
   `want_reply` was set — same convention `pump` uses for keepalives) before
   falling through to the existing `CHANNEL_OPEN_CONFIRMATION` /
   `CHANNEL_OPEN_FAILURE` match.

2. **`expect_channel_reply`** (used by `pty-req`, `exec`, `shell`): same
   `SSH_MSG_GLOBAL_REQUEST` handling, plus `SSH_MSG_CHANNEL_WINDOW_ADJUST`.
   The window adjust can't just be discarded — dropping it would mean the
   interactive phase starts with a stale, too-small `send_window`, stalling
   output the moment the *real* window fills up — so the function now takes
   the channel id and `&mut u32` window by reference and applies the credit:

   ```rust
   SSH_MSG_CHANNEL_WINDOW_ADJUST => {
       let mut off = 0;
       if read_u32(&payload, &mut off) == Some(channel)
           && let Some(add) = read_u32(&payload, &mut off)
       {
           *send_window = send_window.saturating_add(add);
       }
   }
   ```

   This required threading `send_window` as `mut` from its origin (the
   `CHANNEL_OPEN_CONFIRMATION` destructure) through all three call sites,
   since a window adjust can arrive before *any* of `pty-req`, `exec`, or
   `shell` gets its reply.

Both sites now match the pattern `pump`'s main loop already used — this is a
consolidation of message handling onto one convention, not a new one.

## 4. Verification

No host-testable surface here — this is wire-protocol sequencing against a
live peer's exact timing, which `client_wire`'s packet-framing unit tests
don't exercise. Verified against a real OpenSSH server (`linuxserver/openssh-server`
in Docker, reached from the guest over the QEMU SLIRP gateway at `10.0.2.2`),
with the client's identity key (`/etc/sshd/id_ed25519`) added to the
container's `authorized_keys`:

```
$ ssh -p 2299 -l tester 10.0.2.2 echo hello_from_real_sshd
[ssh] connecting to 10.0.2.2:2299...
[ssh] no /root/.ssh/id_ed25519; using sshd's host key (/etc/sshd/id_ed25519) as identity
hello_from_real_sshd
```

- **One-shot `exec`** (above) — host-key TOFU, publickey auth, `exec`
  channel request, output, clean exit. Previously failed at message type 80.
- **Interactive `pty`** — `echo pty_ok`, prompt, `exit` all round-tripped
  correctly over the same connection. Previously failed at message type 93
  (the `pty-req` reply).
- **Regression check** — akuma-to-akuma (`ssh` client → this repo's own
  `sshd`) re-verified working after the change, both with a rejected identity
  (clean auth-failure error, unchanged) and with an authorized one (`exec`
  round-trip, unchanged).
- Host unit tests (`cargo test -p sshd --lib --no-default-features`): 29/29.
- `cargo clippy -p sshd --bin ssh --release --target aarch64-unknown-none`:
  clean (one pre-existing, unrelated `too_many_arguments` warning on
  `client_wire::kex_exchange_hash`).

## Background

- [`SSH_CLIENT.md`](SSH_CLIENT.md) — client scope, usage, identity keys,
  `known_hosts`; the "Interactive pump" section documents the
  already-correct `SSH_MSG_GLOBAL_REQUEST` handling in `pump` that the
  handshake-phase code now matches.
- [`FLOW.md`](FLOW.md) — session/channel lifecycle (server side, but the
  message types are the same wire format both directions).
