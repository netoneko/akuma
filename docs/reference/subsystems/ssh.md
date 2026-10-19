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

### Session lifetime: grace + idle watch (the 2026-10-15 half-open lockout)

> Observed live on ryzen boot 120 (entry 20, the `powersaved` metal run;
> `overlays/ryzen/README.md`): sshd answered TCP and served its banner for
> hours but refused every new session —
> `[SSHD] Refusing connection: 24 sessions already live (max_sessions=24)`.
> The wifi link had gone dormant with no traffic; clients dying mid-key-exchange
> left half-open sessions that nothing ever retired. The kernel itself was
> healthy the whole time (steady IDLESTAT cadence to the end of `klog-120`).
> Analysis + fix: `userspace/sshd/src/session_policy.rs` (decisions) and
> `userspace/sshd/src/protocol.rs` (effects).

**Why the box looked alive while nobody could get in.** The kernel's TCP
stack completes the handshake before sshd is ever consulted — the accept
backlog lives in `akuma-net` — so port checks and `wifi-ssh.py` saw an open
port the whole time. The version string, though, is *application* data: a
connection refused at the cap is dropped by `serve()` before
`handle_connection` ever runs, so the client never sees
`SSH-2.0-Akuma_0.1` — it gets a bare EOF (the repro's
`REFUSED (EOF at cap, no banner)` below; "the banner still answered" in the
boot-120 writeup describes the connections that got through *before* the
slot pool filled). So the lockout's outside signature is: TCP connects, SSH
does not — which reads as a flaky link, not a dead daemon, and is exactly
why the incident looked like "ssh flaked" until the serial log showed the
refusal wall.

**The lifecycle, and which transitions had no timer before the fix.** A
session is created at `accept` → `handle_connection` (`SshSession::new`, which
is where the slot is paid for — a future in the cooperative build, a forked
process in the default build). It advances through the version line →
`KEXINIT` → `KEX_ECDH_INIT`/`NEWKEYS` → `SERVICE_REQUEST` → `USERAUTH_REQUEST`
(auth success flips `SshState::Authenticated`), then a `shell`/`exec` channel
request hands it to `bridge_process` for its whole remaining life. Retirement
paths: peer EOF/RST on the socket, `SSH_MSG_DISCONNECT`, a write error, the
shell exiting (post-bridge), and the fork build's `wait_any` reap. What had
**no timer at all**:

1. **pre-auth stall** — anywhere from the version line to auth success, a
   session could sit forever. ~24 of these was the boot-120 lockout: every
   leaked slot was pre-auth, dead mid-kex.
2. **vanished client, post-auth** — an established session whose peer went
   away silently (dormant radio, NAT dropped, client killed without FIN)
   waits on its socket forever; a write into the void may never error, which
   is also why boot 120 kept one stuck `/bin/cat` (PID 904) alive behind a
   never-closed channel — the same shape, one layer up: the session's *child*
   had no deadline either.

**The fix — OpenSSH's two mechanisms, not an idle-kill:**

- **`LoginGraceTime` analog** (`login_grace_secs`, default 120 = OpenSSH's
  default): a connection that has not authenticated within the window is torn
  down. This alone reclaims the entire boot-120 leak mass.
- **`ClientAliveInterval`/`CountMax` analog** (`idle_timeout_secs`, default
  600; probe grace 90 s): after 10 min of *total inbound silence* on an
  authenticated session sshd sends a `keepalive@akuma.os` global request
  (`want_reply`). Any RFC 4254 §4 client — OpenSSH's, and this repo's own
  `ssh` — answers an unrecognized request with `REQUEST_FAILURE`, and *that
  reply is inbound bytes*, which resets the watch: a healthy client survives
  arbitrarily long silence, including a one-way transfer where only ACKs flow
  back. A peer whose radio is gone never answers and is reaped ~11½ min after
  its last byte. Probing before reaping is what separates this from a bare
  idle-kill, which would murder a healthy session whose user walked away.

The **decisions** are pure functions over timestamps in the package's host
tested lib target — `session_policy::{grace_expired, idle_probe_due,
idle_dead}` plus the `SessionWatch` stamp struct (two `u64`s and a bool in the
existing session struct; no allocation, per the extreme-size sizing rule) —
and `protocol.rs` only carries the effects. The checks run at every suspend
point: the read path (re-entered each 1 ms executor tick, so a session parked
on `WouldBlock` still ages), the write path (`write_all_with_lifetime`, so a
write that suspends into a full send buffer errors at the next tick instead of
parking forever), and the `bridge_process` loop. When the watch tears a
bridged session down it also `SIGKILL`s the child and closes the fds — the
stuck-`/bin/cat` shape is closed with the same stroke. Both serve shapes
(cooperative and `fork-sessions`) run the same `handle_connection` code, so
both are covered.

**Pre-auth slots still count against `max_sessions` — deliberately.**
OpenSSH splits pre-auth connections out (`MaxStartups`, a separate smaller
cap) because its pre-auth cost is a socket and a state machine. Here a
pre-auth connection already *is* a forked process (the default build forks at
accept, before auth), so the process-table cost the cap exists to bound
(`MAX_PROCESSES = 64`, two slots per occupied session) is paid at accept: a
separate pre-auth cap would rename the pressure, not reduce it. What makes
full-cap pre-auth safe is the grace timer recycling the slots — OpenSSH pairs
`MaxStartups` with `LoginGraceTime` for the same reason. (A pre-auth flood
that *never* completes auth now holds slots for at most `login_grace_secs`
each.)

**The kernel half of the honest fix: keepalive exists; its detection leg
does not — by documented decision.** `crates/akuma-net` does expose
`SO_KEEPALIVE` (`set_socket_keepalive`, `socket.rs`), and since
`docs/archive/DEVBOX_ISSUES.md` Issue 19 it actually arms smoltcp's
`set_keep_alive` (7200 s interval, `KEEPALIVE_IDLE_SECS`). But smoltcp
keepalive only *sends probes* — "the remote endpoint … does not answer" is
something the socket never notices, unless `Socket::set_timeout` is also set,
and `crates/akuma-net/src/smoltcp_net/consts.rs` records that `set_timeout`
is **deliberately not used**: it is an inactivity ceiling that aborts idle
*healthy* connections too (an ssh prompt, a keep-alive pool), the exact
failure class `docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md` spent four
bugs eliminating. Nothing in userspace can even ask for keepalive today
(`libakuma` wraps no `setsockopt`). So arming keepalive from sshd would add
nothing observable, and the daemon-side timers above are the fix. What the
kernel-side completion would take, for the record: a probe-failure counter
(Linux's `tcp_keepalive_probes` analog) in `akuma-net`'s TCP poll — declare
the connection dead after N unanswered probes rather than arming the blunt
`set_timeout` — plus a `setsockopt` wrapper in `libakuma`. That is
amd64 + aarch64 shared kernel work (the smoltcp stack is the box-0 stack on
both; no AArch64-only impact, but the fast lane cannot exercise it — it needs
a real TCP peer, so `/root/taprun.sh` on the trashcan is the honest rig) and
it buys minutes at best over the userspace watch, which already bounds the
damage. Not taken.

Both timers are configurable (`sshd.conf` `login_grace_secs=` /
`idle_timeout_secs=`, CLI `--login-grace-secs` / `--idle-timeout-secs`; 0
disables either, OpenSSH's `0` spelling) — and disabling the grace reopens
the leak, so the defaults stay on.

#### Reproduction (amd64 QEMU, `hostfwd` 2222 — no radio needed)

Both runs boot `INIT=/bin/sshd` on `sh amd64/run.sh` and drive the same
probe (30 TCP connections to port 2222, read the banner, send nothing —
the wifi flake is not part of the bug). With the fix, sshd also runs
`--login-grace-secs 30 --idle-timeout-secs 60` purely to make the transcript
human-scale; defaults are 120/600.

**Before** (unfixed binary — it does not know the flags:
`[SSHD] Unknown argument: --login-grace-secs`): 24 of 30 connections take a
slot, 6 are refused at the cap, and **40 s later the box is still locked** —
with no grace timer, "40 s" and "4 h" are the same picture (this is boot
120's multi-hour outage):

```text
== held 30 connections: 24 accepted, 6 refused
== holding 40s ...
== recheck after 40s: REFUSED — lockout persists
```

```text
$ grep -c "Closing connection\|torn down" /tmp/akuma-before-boot.log
0
```

**After** (fixed binary, same probe): the same 24/6 split — the cap still
exists — then the grace timer reclaims every pre-auth slot and a fresh
client gets in:

```text
== held 30 connections: 24 accepted, 6 refused
== holding 40s ...
== recheck after 40s: ACCEPTED (slot freed)
```

```text
[SSHD] Closing connection: login grace expired (no auth within the window)   ×24
[SSHD] Accepted connection                                                    ← the recheck
```

A real login still works against the fixed daemon (macOS `ssh`, the staged
test key): `Signature verified successfully` → `LOGIN-OK`. Two further
transcripts pin the idle watch's two sides, using a *stopped* client
(`SIGSTOP`: TCP alive, cannot answer — a stand-in for the vanished radio):

```text
[SSH] idle: sending client-alive probe                                        ← T+60s silence
[SSHD] Session pid 64 torn down by the idle watch: no inbound bytes for 150s (probe unanswered)
```

and on that dead session's client, unfrozen later:
`Connection to localhost closed by remote host.` — the child (`sleep 600`,
pid 64) was SIGKILLed with the session, so nothing is stranded.

A **healthy** client answers the probe and is left alone: with the same
60 s idle timeout, a live session sat silent past the deadline —

```text
debug1: client_input_global_request: rtype keepalive@akuma.os want_reply 1    ← macOS client, T+60s
```

— and was still connected and executing commands a minute later (killed
locally for the transcript, exit 124 from `timeout`): the watch reset on the
client's `REQUEST_FAILURE` and no reap line ever appeared for it. Probe,
then reap — never a bare idle-kill.



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
- **PTY / winsize:** `TerminalState` (`crates/akuma-tty/src/session.rs` —
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
