# The `ssh` client's host-key prompt ignored Enter — `\r` is a line terminator too

**Date:** 2026-09-10
**Status:** FIXED (`userspace/sshd/src/client/protocol.rs`, `prompt_yes_no`).
**Symptom:** connecting to a new host from the guest's own `ssh` client prints
the TOFU prompt and then **nothing happens**. Typing `yes` and pressing Enter
does nothing, forever. The session looks dead.
**Grade:** A. One-line cause, three-case regression check, verified on QEMU and
bare metal.

```
/ # ssh late.sh
[ssh] connecting to 46.62.210.86:22...
The authenticity of host 'late.sh:22' can't be established.
ED25519 key fingerprint is SHA256:Tc7aeaaQulKJeF3C/zd2BKvq/KgaQTHyMPNa3s9K+aE=.
Are you sure you want to continue connecting (yes/no)?
```

## 1. The cause

```rust
if byte[0] == b'\n' { break; }                                  // only LF ends it
if byte[0] != b'\r' && line.len() < 16 { line.push(byte[0]); }  // CR discarded
```

**A terminal in raw mode sends CR for Enter.** The loop threw it away and kept
waiting for an LF that was never coming. It had also consumed the `yes`, so
there was nothing left to show for it.

## 2. Why the serial console worked and ssh did not

This is the interesting half, and it is the same console/pipe split the whole
4b fold series keeps meeting — from the other side.

| | serial console | over an ssh session |
|---|---|---|
| the client's fd 0 | `FileDescriptor::Stdin` | a `PipeRead` |
| who serves the read | `amd64::fd::read_console` | `akuma-syscalls-glue`'s pipe arm |
| line discipline | **yes** — `TerminalState::map_cr_to_nl` | **none**; a pipe has no discipline on either kernel |
| Enter arrives as | `\n` | `\r` |

So the console converted Enter for it and the prompt worked; over ssh the `\r`
arrived verbatim and was dropped. The kernel is correct in both cases — there is
no line discipline on a pipe on Linux either.

**And that is why `vi` over the same session was fine while this was not.** `vi`
reads raw keys and interprets them itself; this wanted one specific byte. The
user's observation — *"what I find fascinating is that vi works with correct
input but ssh does not"* — is what turned a week-long-looking bug into a
one-line one, because it ruled out the transport and the kernel in a sentence.

## 3. Why every earlier test passed

Every harness in this session piped `printf 'yes\n'` — a literal LF, **which no
keyboard produces in raw mode**. Measured either side of the change, same guest,
same server, only the terminator differing:

| input | before | after |
|---|---|---|
| `yes\r` | **hangs forever** (45 s timeout, no further output) | echoed, accepted, proceeds |
| `yes\n` | works | works |
| `nope\r` | hangs | `ssh: host key not accepted` |

The lesson worth keeping: **a test that types `\n` at an interactive prompt is
not testing the interactive prompt.** Send `\r`, which is what Enter is on the
wire, and keep the `\n` case as the console/cooked arm.

## 4. The echo is part of the fix, not polish

Nothing echoed at that prompt. Real `ssh` relies on the *local* tty having
`ECHO` set, but an interactive session reaches this process as a raw pipe with
the operator's own terminal in raw mode, so the guest has to echo or nobody
does. Without it the operator types blind and cannot tell a working prompt from
a wedged one — which is precisely how this was reported, and why the first
diagnosis was "input never arrives" rather than "the terminator is wrong".

`prompt_yes_no` now echoes each accepted byte and emits `\r\n` on the
terminator. Backspace is deliberately not handled: the answers are three
characters and a wrong one is safe, because it declines.

A CRLF pair leaves its `\n` unread, which the shell then sees as an empty
command line — one spurious prompt, and cheaper than a non-blocking peek to
consume it.

## 5. What was *not* the cause — two retractions worth keeping

Both of these were believed mid-investigation and both are wrong. They are
recorded because each cost real time and each would have sent the next reader
somewhere useless.

- **Not the `O_NONBLOCK` console fix** (`AMD64_CONSOLE_NONBLOCK_READ.md`, same
  day). That was a real kernel bug on a real path, but it is not this one: the
  prompt is at `protocol.rs:240` and the client only sets stdin non-blocking at
  ~376, *after* it. `read` returns `isize`, so an `EAGAIN` there would have
  returned `false` and printed `host key not accepted` — an abort, not a hang.
  The two bugs are independent and both had to be fixed for typing to work.
- **`unlink` is NOT broken on the metal.** Mid-investigation `rm -f
  /root/.ssh/known_hosts` twice appeared to succeed and leave the file in
  place — 98 bytes, mtime `Jan 1 1970`, which reads exactly like a file baked
  into the image on a read-only medium. It was neither. **The client had
  recreated it**: the test run between the `rm` and the `ls` accepted a host key,
  and `add_known_host` wrote it straight back. The 1970 mtime is just the
  missing clock at write time. Directly disproved afterwards on the same box:
  `echo hi > /tmp/u1; rm /tmp/u1` → `rc=0`, gone. **A file that reappears is not
  a failed delete until you have accounted for every writer.**

## 6. Still open, and adjacent

- **`add_known_host` writes to `$HOME/.ssh/`, which may not exist.** Over an ssh
  exec channel the client reported `no /root/.ssh/id_ed25519; using sshd's host
  key as identity`, and on a fresh boot `/root/.ssh/known_hosts` is absent — so
  on some paths the TOFU record does not persist and the prompt returns every
  connection. Worth making the directory rather than failing quietly; the
  function's own doc comment already says a silent failure here "reads as 'the
  host key changed' if the operator isn't watching closely".
- **The prompt string reaches the terminal late.** `print` is an unbuffered
  `write(1, …)`, so the delay is the guest **sshd**'s channel bridge not
  forwarding a partial line (no newline) until more output arrives. Measured on
  QEMU: the fingerprint lines appeared at 0.15 s and the `Are you sure …?` line
  only at 19.98 s, bundled with the next write. Cosmetic here, but it is the
  same class as `SSHD_DRAIN_FIX` and it makes every interactive prompt in a
  session look stalled.
- **A blocked `read` on a pipe whose writer is gone never returns EOF.** A test
  client left parked at this prompt was still in `ps` hours after the ssh
  session feeding its stdin had died. Nothing could reap it either, because
  `kill` is `ENOSYS` on this target. Build pipelines (`cmd | cmd`) depend on
  EOF to terminate, so this is a hang class on the path to self-hosting, not
  just a stale-process annoyance.

## Verify

```bash
# On the guest, against any host not in known_hosts. The terminator is the test:
#   yes\r  must be accepted (raw-mode Enter)
#   yes\n  must be accepted (cooked/console Enter)
#   nope\r must decline
( printf 'ssh <host>\n'; sleep 16; printf 'yes\r'; sleep 14 ) \
  | ssh -tt akuma
# expect:  Are you sure you want to continue connecting (yes/no)? yes
#          ...proceeds to identity + auth
```

| gate | result |
|---|---|
| `yes\r` / `yes\n` / `nope\r`, QEMU | **accept · accept · decline** |
| `yes\r`, **bare metal** (HP 500-502nj) | **accept**, echoed |
| bare-metal self-tests after restage | **616 passed, 0 failed** |
| `cargo test -p sshd --lib --no-default-features` (host) | **29/29** |
| `cargo clippy -p sshd --target x86_64-unknown-none` | clean (the one warning, `kex_exchange_hash` arg count, is pre-existing in `client_wire.rs`) |

## Background

- `docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md` — the same day's kernel-side
  fix on the same feature, and §5's first retraction.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH4B.md` § 2.1 — the two
  interactive-shell architectures, and why a pipe is the terminal on this
  target.
- `docs/archive/TTY_SHENANIGANS.md` — the AArch64 side's own history with
  cooked-vs-raw over a channel.
- `crates/akuma-terminal/src/lib.rs` — `map_cr_to_nl`, the function whose
  presence on one path and absence on the other is the whole of §2.
