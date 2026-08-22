# SSH client (`ssh`)

A minimal interactive SSH-2 client, built as a second binary target
(`ssh`) in the `sshd` package rather than a separate crate — the two sides
speak opposite halves of the same wire format, so it made sense to keep them
together. Source under `userspace/sshd/src/client/`; the shared,
host-testable wire-format pieces live in `userspace/sshd/src/client_wire.rs`
(part of the `sshd` **lib** target).

For the algorithm suite and the shared crypto crate, see
[`../../../docs/reference/subsystems/ssh.md`](../../../docs/reference/subsystems/ssh.md).
For the security posture specifically, see
[`SECURITY_IMPROVEMENTS.md`](SECURITY_IMPROVEMENTS.md).

## Why it exists

`userspace/sshd` only ever had a server. The client was added so an Akuma
box can reach *other* SSH servers — including real-world ones with an
actual auth story, like [late.sh](https://late.sh) (an SSH BBS: `ssh
late.sh`, no password, your key is your identity) — not just talk to its own
`sshd`. That external-interop requirement is why this client, unlike
`sshd`, implements real flow control and a TOFU `known_hosts`: those matter
against a server we don't control, even if they're moot in a same-repo
client/server pair.

## Scope

Deliberately narrow — "terminal features only":

| | |
| --- | --- |
| Key exchange | `curve25519-sha256` (exact match required; no negotiation) |
| Host key | `ssh-ed25519` only |
| Cipher | `aes128-ctr` (both directions) |
| MAC | `hmac-sha2-256` (both directions) |
| Auth | `publickey` (`ssh-ed25519`); queries `none` first for a clear error if the server doesn't offer publickey |
| Session | Interactive shell over a `pty` (default), or one-shot `exec` (when a command is given — no pty) |
| Rekeying | None — one KEX per connection |
| Forwarding | None — no `-L`/`-R`/`-D`, no agent forwarding, no X11 |
| Subsystems | None — no SFTP, no SCP |

If the peer's KEXINIT doesn't advertise all four required algorithms, the
client fails fast with a clear message naming which one and why, rather than
attempting any fallback.

## Usage

```
ssh [-p port] [-l user] [-i identity_file] [-t term] [user@]host [command...]
```

- No command → interactive shell (`pty-req` + `shell` channel request).
- A command → one-shot `exec` (no pty), exits with the remote command's
  status (or 255 if no `exit-status`/`exit-signal` report ever arrived —
  same convention as OpenSSH and as this repo's own `sshd`, see its
  `send_exit_report` doc comment).
- Default port 22 (not `sshd`'s dev default of 2222 — this is a client that
  needs to reach arbitrary real-world servers); default user `root`.

## Identity key

Precedence, first match wins:

1. `-i <path>` if given.
2. `$HOME/.ssh/id_ed25519` (`$HOME` defaults to `/root`).
3. `/etc/sshd/id_ed25519` — `sshd`'s own host key, reused as the client's
   identity so a fresh box can `ssh` out without a separate key-generation
   step.
4. Generate a new key and persist it to `$HOME/.ssh/id_ed25519` (plus a
   `.pub` file in the usual `ssh-ed25519 BASE64` format).

**Format note:** identity files are Akuma's raw 32-byte secret-key format —
the same one `sshd` writes to `/etc/sshd/id_ed25519` — not OpenSSH's PEM
`-----BEGIN OPENSSH PRIVATE KEY-----` container. A key from real
`ssh-keygen` will not load here. Supporting that format (plus its optional
bcrypt-KDF encryption) was judged out of scope for a minimal client; point
`-i` at a raw key instead, or let the client generate its own.

## `known_hosts`

Trust-on-first-use, at `$HOME/.ssh/known_hosts`, lines of
`host:port ssh-ed25519 BASE64`:

- **New host:** prints the `SHA256:...` fingerprint and prompts
  `yes`/`no` on the local terminal (still in cooked mode at this point —
  raw mode isn't entered until after the channel is open). EOF or any read
  error on the prompt is treated as "no" — fail closed, never fail open.
- **Known, matching:** silent, connects.
- **Known, mismatched:** hard refusal with a loud warning (mirrors
  OpenSSH's "REMOTE HOST IDENTIFICATION HAS CHANGED"). This is on top of —
  not instead of — verifying the KEX exchange-hash signature against the
  offered host key; TOFU only adds *identity pinning* across connections,
  it isn't what makes the handshake itself trustworthy.

## Interactive pump

Once the channel is open, the client enters a single-threaded, non-blocking
poll loop (`protocol::pump`) mirroring `sshd`'s own `bridge_process`:

- Local stdin ↔ `CHANNEL_DATA`, honoring the peer's advertised channel
  window and max-packet-size on the way out (chunks large writes, stalls
  when the window is exhausted rather than overrunning it), and returning
  window credit on the way in once it builds up past 64 KiB.
- `exit-status` / `exit-signal` channel requests set the process's own exit
  code.
- `CHANNEL_EOF` from the peer doesn't end the session — output keeps
  draining until `CHANNEL_CLOSE`, so a command's last bytes of output are
  never lost.
- Local EOF (piped stdin ending) sends `CHANNEL_EOF` once the queued bytes
  have actually drained to the peer, then keeps pumping remote output.
- Unsolicited `SSH_MSG_GLOBAL_REQUEST`s (e.g. a keepalive) get an
  `SSH_MSG_REQUEST_FAILURE` if they asked for a reply, so a long idle
  session doesn't look unresponsive to a peer that probes for one.

Raw terminal mode (`akuma_terminal::mode_flags::RAW_MODE_ENABLE`) is only
entered for a `pty` session (i.e. no command given), and is always restored
before the process exits, on every return path out of `run()` — see
`SECURITY_IMPROVEMENTS.md` for why that has to be an explicit call rather
than `Drop`-based cleanup on this target.

## Host-tested pieces

`src/client/main.rs` (the binary) links `libakuma` unconditionally, so it
can't be host-tested — same reason `sshd`'s own `main.rs` can't. The pure
byte-handling pieces (KEXINIT build/parse, the KEX exchange-hash
computation, and encrypted/unencrypted packet framing) live instead in
`sshd::client_wire` (the package's **lib** target), and are exercised by 29
tests covering round-trips, partial-buffer handling, and — see
`SECURITY_IMPROVEMENTS.md` — several buffer-safety regressions caught while
writing them:

```bash
cd userspace && cargo test -p sshd --lib --no-default-features \
    --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

## Build

```bash
cd userspace && ./build.sh --ssh-only     # -> bootstrap/bin/ssh
```

`ssh` is declared as a second `[[bin]]` in `sshd`'s `Cargo.toml`
(`src/client/main.rs`), not a package of its own, so `cargo build -p ssh`
does not resolve — `build.sh --ssh-only` knows to build the `sshd` package
(which produces both binaries) and copy out just `ssh`. A plain
`cargo build --release -p sshd` (or the full `userspace/build.sh`) already
produces both `sshd` and `ssh` for free.
