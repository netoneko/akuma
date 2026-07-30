# Remote command exit codes: `ssh host cmd` always returned 255

## 1. Symptom

Every remote command reported exit status **255**, whether it succeeded or not:

```
$ ssh -p 2222 root@localhost true      ; echo $?    # 255  (should be 0)
$ ssh -p 2222 root@localhost 'echo HI' ; echo $?    # HI, then 255  (should be 0)
$ ssh -p 2222 root@localhost 'exit 42' ; echo $?    # 255  (should be 42)
```

The output was correct — `echo HI` printed `HI` — so the bridge and channel
plumbing were fine. Only the *status* was wrong, and it was wrong in a way that
made every scripted use of Akuma's sshd look like a connection failure: 255 is
the code OpenSSH uses for "the connection itself failed", so `ssh box make` in a
CI step could not tell a broken build from a broken network.

## 2. Root cause

Akuma's sshd never sent the `exit-status` channel request. It closed the
connection after the shell exited and left the client to guess.

There is nothing to guess with. Per RFC 4254 §6.10 the **server** is the only
party that knows the remote exit code, and it must say so explicitly:

```
byte      SSH_MSG_CHANNEL_REQUEST (98)
uint32    recipient channel
string    "exit-status"
boolean   want_reply   — MUST be false
uint32    exit_status
```

OpenSSH's client seeds its own exit status with 255 up front and overwrites it
only when that request arrives. No request → the 255 placeholder is what the
client returns. So this presented as "sshd returns the wrong code" when in fact
sshd returned *no* code.

The exit code was not merely unsent — it was thrown away at the source. In
`bridge_process` the reap discarded it in the binding:

```rust
if let Some((_, _exit_code)) = waitpid(pid) {   // ← dropped on the floor
```

## 3. Fix

`userspace/sshd/src/protocol.rs`, `userspace/sshd/src/wire.rs`.

1. **Keep the code.** The bridge loop is now an expression that evaluates to the
   child's exit code (`break code`), so the value the reap already had reaches
   the end of the session instead of being dropped. Using the loop's value —
   rather than a `let mut exit_code = 0` written from inside — means there is no
   "default 0" that a future early `break` could silently report as success.

2. **Send it.** A new `send_exit_status` emits the §6.10 request followed by
   `CHANNEL_EOF` and `CHANNEL_CLOSE`, then clears `channel_open` so the channel
   can't be closed twice. It runs *after* the stdout drain (so the status can
   never overtake the command's output) and *after* the fds are closed (so a
   write error propagating with `?` cannot leak them).

3. **Spawn failures get 127**, the shell convention for "command not found",
   rather than falling back to the ambiguous 255.

Interactive `ssh -tt` sessions were affected identically and are fixed by the
same change, because both paths share `bridge_process`.

### Why `wire.rs` exists

The message-building is split into `src/wire.rs` — pure `Vec<u8>` construction,
no session, no socket, no syscalls — purely so it can be unit-tested on the
host. See §4.

## 4. Tests

**Host unit tests** — `src/wire.rs`, 6 tests. sshd had none before this; the
crate is `no_std` + `no_main` for `aarch64-unknown-none` and links `libakuma`,
whose `#[panic_handler]` and `#[global_allocator]` collide with std's, so no
target of this package could previously be compiled for a std host. Two changes
open the door:

- `libakuma` became an **optional** dependency behind the default `akuma`
  feature, so `--no-default-features` leaves it out of the build graph.
- A `src/lib.rs` lib target carries `wire` and is `#![cfg_attr(not(test),
  no_std)]`, so the test harness may link std while the shipped build stays
  `no_std`.

```bash
cd userspace
cargo test -p sshd --lib --no-default-features \
    --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

The tests assert the exact §6.10 byte layout (spelled out literally, not rebuilt
with the same helpers under test), that `want_reply` stays false, a round-trip
through the reader functions a client would use, low-byte masking of the code
(256 → 0, -1 → 255), and that the three teardown messages keep distinct tags.

They were mutation-checked — `want_reply = 1`, dropping the `& 0xFF` mask, and
sending `CLOSE` with the `EOF` tag each fail at least one test.

This covers the wire format only. Nothing host-testable reaches `bridge_process`
itself (it is syscalls end to end), so the reap-and-report path is covered by §5.

## 5. Verification

A/B on one VM, both sshd builds live at once: the pre-fix binary on `:2300`, the
fixed one on `:2299`.

```
cmd                  OLD(:2300)      NEW(:2299)
true                 rc=255          rc=0
exit 42              rc=255          rc=42
echo HI              rc=255  HI      rc=0   HI
false                rc=255          rc=1
exit 7               rc=255          rc=7
```

Full suite against the fixed build — 8/8:

| command | expected | got |
| --- | --- | --- |
| `true` | 0 | 0 |
| `exit 0` | 0 | 0 |
| `echo HELLO_EXIT_TEST` | 0 + output | 0 + output |
| `false` | 1 | 1 |
| `exit 42` | 42 | 42 |
| `exit 7` | 7 | 7 |
| `sh -c 'exit 3'` | 3 | 3 |
| `/nonexistent/binary` | 127 | 127 |

Interactive `ssh -tt` (stdin piped, shared `bridge_process` path) — 3/3:
`echo MARKER_A; exit 5` → 5 with `MARKER_A` present, `exit` → 0,
`false; exit $?` → 1.

Host unit tests: 6/6. Target build clean (no new warnings).

## 6. Known gap

A shell **killed by a signal** is reported as its exit code (0) instead of the
`exit-signal` request of RFC 4254 §6.10. `libakuma::waitpid` decodes only
`WEXITSTATUS` and discards the raw wait status, so sshd cannot currently
distinguish the two — closing that gap means changing `libakuma`, outside this
fix. Recorded in `LIMITATIONS.md` §6.

## Background

- `INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md` — the earlier fix to the same
  `bridge_process` loop, covering *output* lost at child exit. This one covers
  the *status* lost at the same point.
- `FLOW.md` — session and channel lifecycle.
