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

### Signal path

`ssh -v host 'kill -9 $$'` — the client's own trace is the proof the request
arrived and parsed, rather than the 255 default being left in place (both give
exit 255, so the exit code alone proves nothing):

```
debug1: client_input_channel_req: channel 0 rtype exit-signal reply 0
debug1: Exit status -1
```

`reply 0` confirms `want_reply` is false as §6.10 requires; `Exit status -1` is
OpenSSH's internal marker for a signal death. sshd's own log for the same run:

```
[SSH] Exec: /bin/sh ["-c", "kill -9 $$"]
[SSH] Killed by signal 9 (KILL)
```

Delivery-race fix: **25/25** SIGKILL runs delivered `exit-signal`, including
across 45-second idle gaps (the condition the original miss appeared under),
versus ~1 in 10 missing before. `exit-status` (8/8) and interactive (3/3)
re-checked after the change.

Host unit tests: 11/11. Target build clean (no new warnings).

## 6. Signal deaths: `exit-signal`

Initially a shell **killed by a signal** was still reported via `exit-status`,
which is wrong in the worst direction: `WEXITSTATUS` for a signal death is 0, so
a killed command looked like a clean success.

The kernel was never the problem. It already distinguishes the two cases —
`kill_process` records `exit_code = -9`, `terminate_process_with_signal` records
`-(sig)`, and `encode_wait_status` (`src/syscall/proc.rs`) puts a clean exit in
the high byte and a signal in the low 7 bits, with boot self-tests covering it.
The information was being discarded one layer up, in `libakuma::waitpid`, which
returned only `(status >> 8) & 0xFF`.

**`libakuma`** (`userspace/libakuma/src/lib.rs`) gained a `WaitStatus` type
carrying the raw status word plus `exited()` / `exit_code()` / `signaled()` /
`term_signal()` / `shell_code()`, and `waitpid_status()` returning it.
`waitpid()` is now a thin wrapper over it, keeping its exact previous signature
and semantics so the other 11 call sites (`box`, `herd`, `httpd`, `elftest`,
`meow`) are unaffected — its doc comment now warns that its exit code is
`WEXITSTATUS` only.

**sshd** uses `waitpid_status` and picks the matching §6.10 report:
`exit_signal_payload` names the signal (RFC signal names, no `SIG` prefix;
anything unlisted uses the `@domain` extension form rather than being forced onto
a wrong standard name), sets `core_dumped` false, and is sent *instead of*
`exit-status`, never both.

Note what this correctly does **not** catch: a shell that *handles* a signal and
then exits normally has no signal death to report. busybox `sh` does exactly this
for SIGTERM/SIGINT/SIGQUIT/SIGSEGV — it exits 130 — so those arrive as
`exit-status 130`, which is the truth about what the shell did. SIGKILL cannot be
caught, so it is the case that exercises the `exit-signal` path.

### The delivery race this exposed

Once the exit report existed, a second bug became reachable: roughly 1 in 10
signal-killed commands arrived with **no** request at all, sshd's log showing it
had sent one. `write_all` only *queues* bytes in the TCP stack, and returning
from the session drops `SshStream`, closing the socket — a close that races the
flush discards the just-queued packets, and the client falls back to its 255
placeholder.

`send_exit_report` now waits (bounded, cooperatively, via `yield_now`) for the
client's `CHANNEL_CLOSE` or hangup before returning, which keeps the socket alive
until the report has actually gone out. This was always latent; before the fix
there were simply no final packets to lose.

## Background

- `INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md` — the earlier fix to the same
  `bridge_process` loop, covering *output* lost at child exit. This one covers
  the *status* lost at the same point.
- `FLOW.md` — session and channel lifecycle.
