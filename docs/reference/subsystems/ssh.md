# SSH

Current-state architecture for Akuma's SSH server *and* client, both in
`userspace/sshd` (two binary targets, `sshd` and `ssh`, in one package).
Since 2026-08-10 there is exactly one server: the **userspace `/bin/sshd`**
(`userspace/sshd`). The built-in in-kernel SSH-2 server (`src/ssh/`,
`crates/akuma-ssh`) was deleted from every profile — see
[`../../archive/BUILTIN_SSH_REMOVAL.md`](../../archive/BUILTIN_SSH_REMOVAL.md)
for the measurements that motivated it (217 KB of `extreme` image, +308 KB of
free RAM at the 4 MB floor) and
[`../../archive/IN_KERNEL_SHELL.md`](../../archive/IN_KERNEL_SHELL.md) for the
in-kernel shell that went with it.

> **Stability: B (verify behaviour).** Downgraded from A on 2026-08-10: the
> in-kernel implementation this doc used to cover in parallel is gone, so the
> userspace server is now the only path and has not yet accumulated the same
> soak time as the pair did. The load-bearing invariant is unchanged:
> session futures must use `yield_now()`, never `sleep_ms`, or one session
> starves all the others.

For debugging, see [`../../runbooks/debug-ssh-latency.md`](../../runbooks/debug-ssh-latency.md).

## Algorithm suite

KEX `curve25519-sha256`; host key `ssh-ed25519`; encryption `aes128-ctr`; MAC
`hmac-sha2-256`; compression `none`; auth **publickey (Ed25519 only — RSA
rejected)**. Both `sshd` and `ssh` speak exactly this suite — the client
requires an exact match on all four algorithms and fails fast (naming which
one) rather than negotiating a fallback. Wire/crypto primitives live in the
host-testable `akuma-ssh-crypto` crate (server) and
`userspace/sshd/src/client_wire.rs` (client, in the package's own lib
target). `userspace/sshd` links `akuma-ssh-crypto` with
`default-features = false` + `features = ["zeroize"]`: drops `fast`
(curve25519-dalek's ~30 KB precomputed basepoint table — see
[`../../archive/TRIM_FAT_SSHD.md`](../../archive/TRIM_FAT_SSHD.md); neither
binary signs/verifies often enough for it to pay for itself) but keeps
`zeroize` (key material zeroed on drop) on, since that one's a security
property, not a size trade — see
[`../../../userspace/sshd/docs/SECURITY_IMPROVEMENTS.md`](../../../userspace/sshd/docs/SECURITY_IMPROVEMENTS.md) §10.

## Userspace sshd (`userspace/sshd`)

- Entry `main()` (`userspace/sshd/src/main.rs`): load config + host key, parse
  `--shell`/`--shell-arg`/`--port` (precedence CLI > config > 2222), bind
  `0.0.0.0:port`, listener non-blocking.
- **Cooperative multiplexer:** a `Vec<Pin<Box<dyn Future>>>` holds one future
  per accepted session. Each tick: `listener.try_accept()`, then poll every
  session future once in turn; `Poll::Pending` stays, `Ready` swap-removed. If
  no progress, `sleep_ms(1)`. This lets a second connection progress while the
  first is idle.
- `SshStream` wraps `libakuma::net::TcpStream`: maps `WouldBlock` →
  `Poll::Pending` (re-arming via `cx.waker().wake_by_ref()`).
- **`yield_now()` helper** — **must** be used instead of `sleep_ms` inside
  session futures (`sleep_ms` parks the entire OS thread, starving all other
  sessions).
- Used by **every profile**. There is no other sshd.
- **Started two different ways.** Normally herd launches it from
  `/etc/herd/enabled/sshd.conf` (`--port 22` since 2026-08-10, so host `2222` as
  documented; disks populated before that still say `--port 23`). On
  `extreme + userspace-sshd` there is no herd, and `config::AUTO_START_SSHD`
  spawns `/bin/sshd --port 22 --shell /bin/sh` directly from `kernel_main`.

### Resolved: unauthenticated pre-auth panic

An unchecked `packet_len - padding_len - 1` underflow in the unencrypted packet
path let a single malformed pre-auth packet panic the server. Fixed in
`userspace/sshd` (`userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md`). The in-kernel
copy had the same bug and was strictly worse — `panic=abort` at EL1 has no
process boundary, so a 10-byte crafted packet took the whole VM down — but that
implementation no longer exists, which closes it by deletion.

## Userspace ssh client (`userspace/sshd`, binary `ssh`)

Second `[[bin]]` target in the `sshd` package (`src/client/main.rs`), not a
separate crate — see [`../../../userspace/sshd/docs/SSH_CLIENT.md`](../../../userspace/sshd/docs/SSH_CLIENT.md)
for scope/usage/identity-keys/`known_hosts`, and
[`../../../userspace/sshd/docs/SECURITY_IMPROVEMENTS.md`](../../../userspace/sshd/docs/SECURITY_IMPROVEMENTS.md)
for an audit pass done immediately after writing it (several buffer-safety
panics and a hang class fixed, entropy source for key material fixed, a few
limitations accepted and documented rather than silently left).

- Interactive shell over a `pty` (default) or one-shot `exec` (a command
  given, no pty) — no SFTP/SCP, no port/agent/X11 forwarding, no rekeying.
- **Real flow control** (channel window + max-packet honored both ways) and
  a **TOFU `known_hosts`** (`$HOME/.ssh/known_hosts`, mismatch = hard
  refuse) — both matter for a client reaching a third-party server it
  doesn't control (the motivating case: `ssh late.sh`, an SSH BBS), even
  though they're moot talking to this repo's own `sshd`.
- Identity key precedence: `-i <path>` → `$HOME/.ssh/id_ed25519` →
  `/etc/sshd/id_ed25519` (`sshd`'s own host key, reused) → generate + persist
  to `$HOME/.ssh/id_ed25519`. Raw 32-byte format only, not OpenSSH PEM.
- Host-tested pure wire logic (KEXINIT, exchange-hash, packet framing) lives
  in `sshd::client_wire` (the package lib target) — the `ssh` binary itself
  links `libakuma` unconditionally, same reason `sshd`'s own `main.rs` can't
  be host-tested.

## Auth model

- **Publickey only**, Ed25519 only; `password`/`none` rejected.
- Flow: client `none` → server `FAILURE(methods=publickey)` → client
  `publickey(no sig)` → server checks `authorized_keys` → `PK_OK` → client
  `publickey(sig)` → server verifies against `session_id` → `SUCCESS`.
- `disable_key_verification` in `SshdConfig` returns `Success` unconditionally
  (**test/local-dev only**, e.g. the devbox) — and, since the client's
  security audit, is *also* gated behind the `insecure-disable-key-verification`
  Cargo feature (off by default): without it the config flag is parsed but
  ignored, loudly, so a stray dev config can't silently disable auth in a
  binary nobody built expecting that.
- **Host key:** load 32-byte private key from `/etc/sshd/host_key`; if
  missing/wrong-length, generate via hardware RNG and best-effort persist.
  Public key auto-added to `/etc/sshd/authorized_keys`.

## Terminal handling

- **Rich terminal syscalls 307–313:** `set/get_terminal_attributes` (raw/cooked),
  cursor, clear_screen, `poll_input_event` (blocking/non-blocking/timed). Raw
  mode forwards SSH channel bytes unmodified; cooked mode does line editing +
  echo in the shell.
- **PTY / winsize:** `TerminalState` (`crates/akuma-terminal/src/lib.rs` —
  mode flags, termios `iflag`/`oflag`/`cflag`/`lflag`, the canonical-mode
  line buffer, `c_cc`) is `Arc<Spinlock<…>>` (`term_width`, `term_height`,
  `input_waker`). `TIOCGWINSZ` (`0x5413`) reads; `TIOCSWINSZ`
  (`0x5414`) writes (kernel `src/syscall/term.rs`, the syscall entry point —
  the state itself lives in the crate, not that file). **pty spawn**
  (`SPAWN_FLAG_PTY`): child gets a **fresh** Arc so multiplexed daemons (sshd)
  don't alias `input_waker` slots across sessions — this is why sshd reaches the
  child's state via `TIOCSWINSZ` on the `ChildStdout(pid)` fd rather than its own.
- **Fresh terminal on a box crossing, independent of `pty`.** The inheritance
  decision (`spawn_inherits_terminal`, `crates/akuma-exec/src/process/spawn.rs`)
  is `!pty && box_id == 0`, not just `!pty`. `SPAWN_EXT` — what `box
  run`/herd's per-service launch use — always passes `pty=false` (a boxed
  process's stdin is correctly a plain pipe, not a pty), but before
  2026-08-26 that meant a spawn into a *different* box still inherited the
  caller's `TerminalState` object rather than getting its own. Sharing it
  across the box boundary was a real leak, not just an `isatty()` mismatch:
  every spawn overwrites `foreground_pgid` on whichever object it ends up
  with, so the boxed process's pid silently became the *caller's own
  terminal's* `Ctrl+C` target too, and any raw-mode/`ECHO` ioctl the boxed
  process made mutated the caller's termios right along with it. `box_id ==
  0` (`SPAWN_EXT`'s "stay in the caller's box" case, and every plain
  `sys_spawn`) is unaffected and keeps inheriting, matching a real shell
  subprocess sharing its parent's controlling terminal. Host-tested directly
  (`spawn.rs`'s `terminal_inheritance_tests`) and end-to-end
  (`test_box_crossing_spawn_gets_fresh_terminal_state`,
  `src/process_tests.rs`, `sc-containers`-gated): registers a `TerminalState`
  under the test thread's id, spawns once same-box and once into a fresh
  box, and compares `Arc` identity both times.
- **Key translation** (`akuma_ssh::util::translate_input_keys`): the
  `EscapeState` machine (Normal→Escape→Bracket→`BracketNum(u8)`→Normal) turns
  xterm sequences into actions: Delete `\x1b[3~`, Home `\x1b[1~`, End `\x1b[4~`,
  arrows `\x1b[A/B/C/D`. Window-change requests signal a resize byte to the
  foreground process.
- **Ctrl-C / `SIGINT`:** handled in the kernel, not sshd. `write_to_process_
  stdin` (`crates/akuma-exec/src/process/mod.rs`) — the chokepoint every
  stdin write reaches, sshd's included, via `/proc/<pid>/fd/0` — checks
  `ISIG`/`cc[VINTR]` on a real pty session (`channel.is_terminal()`) and
  broadcasts `SIGINT` to every process sharing the terminal's
  `foreground_pgid` instead of forwarding the byte. See
  [`../../archive/CTRL_C_SIGINT_DELIVERY.md`](../../archive/CTRL_C_SIGINT_DELIVERY.md).

## Background

- `archive/BUILTIN_SSH_REMOVAL.md` — why the built-in server is extreme-only,
  with the size/RAM measurements.
- `archive/TRIM_FAT_SSHD.md` — userspace sshd size work (−24% via dropping
  `curve25519-dalek` precomputed tables).
- `archive/SSH.md`, `archive/SSH_STREAMING_ARCHITECTURE.md`.
- `archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md`, `archive/INTERACTIVE_IO.md`.
- `userspace/sshd/docs/FLOW.md`, `userspace/sshd/docs/LIMITATIONS.md`.
- `userspace/sshd/docs/SSH_CLIENT.md`, `userspace/sshd/docs/SECURITY_IMPROVEMENTS.md`
  — the `ssh` client and its security audit.
