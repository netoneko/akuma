# Exec-channel output over ~1 MiB is silently lossy (and newline-mangled below that)

**Status:** fixed (2026-08-01). Two distinct defects, one kernel-side and one
sshd-side, both pre-existing — reproduced identically on a clean HEAD baseline
boot (stash-discipline A/B) during the Phase 7f BKL work, so neither is related
to the per-syscall opt-out changes. Found while running `md5sum`-based
data-integrity checks over live SSH (`docs/archive/BKL_PHASE7F_OPTOUT_LIST.md`
§2.2). See §7 for what shipped, including a third, previously-latent deadlock
the backpressure fix flushed out.

## 1. Symptom

Piping a large file through a remote command's stdout loses data, silently and
non-deterministically:

```
$ ssh -p 8222 root@localhost "cat /tmp/t1.bin" > local.bin   # t1.bin = 8 MiB
$ wc -c local.bin      # 2.6–3.8 MB, varying per run; ssh exits without error*
```

Three consecutive runs against the same 8 MiB file returned 3,019,417 /
2,661,546 / 2,755,102 bytes (and 2,657,352 / 3,837,518 / 2,722,248 on the HEAD
baseline). Below ~1 MiB nothing is lost, but the bytes still aren't faithful:
a 512 KiB file arrived as **533,650 bytes** (9,362 inserted `\r`s).

\* the tested devbox image's sshd predates the `exit-status` fix
([`EXIT_STATUS_FIX.md`](EXIT_STATUS_FIX.md), commit `e54eba9`) and returns 255
unconditionally, so scripted callers can't even detect the short read.

## 2. The discriminating experiment

Deterministic payload (`seq -w 1 999999`, so every byte's correct value is known
from its offset), 6,999,993 bytes, `cat` over an exec channel (no PTY):

| check | result |
|---|---|
| bytes received | 2,957,215 of 6,999,993 |
| received stream **starts** with | `000001` — the file's first line |
| received stream **ends** with | `999999` — the file's last line |
| last 64 KiB of stream vs last 64 KiB of file | **md5-identical** |
| stream vs same-length file *prefix* | different |

So the loss is **from the middle**: the head survives, the tail survives, and a
varying multi-megabyte span in between is gone. That is not an early channel
close — it is a bounded buffer discarding under pressure.

Control at 512 KiB: after normalizing `\r\n` → `\n`, the received bytes are
**md5-identical to the file**. Below the buffer bound the path is lossless;
the only remaining defect is newline translation.

## 3. Root cause A (kernel): `ProcessChannel` drops oldest on overflow, with no backpressure

The bridge reads the child's stdout through a `FileDescriptor::ChildStdout`
backed by `ProcessChannel`
(`crates/akuma-exec/src/process/channel.rs`). `write_output` caps the buffer at
`MAX_BUFFER_SIZE` (1 MiB) and, on overflow, **drains the oldest unread bytes to
make room**:

```rust
let overflow = (current_len + data_to_write.len()).saturating_sub(MAX_BUFFER_SIZE);
if overflow > 0 {
    buf.drain(..overflow.min(current_len));   // silent data loss
}
buf.extend(data_to_write);
```

The child's `write(1, …)` never blocks or short-writes against this cap, so a
producer that outruns the consumer simply has its oldest output discarded.
`cat` fills 7 MB into the channel near-instantly; the single-threaded
cooperative sshd (see [`LIMITATIONS.md`](LIMITATIONS.md) §1) forwards it over
SSH far slower. Sequence observed in §2: the bridge forwards the first chunks
live (head survives), falls behind, the channel overflows and rotates away the
middle, the child exits, and the exit-path drain flushes the surviving newest
~1 MiB (tail survives). Loss varies run to run with scheduling — exactly the
varying sizes in §1.

This is the same defect class the pipe subsystem already fixed the hard way:
pipes are bounded (`PIPE_CAPACITY`, 64 KiB) but writers **block** when full and
are woken on drain — bounded + backpressure, never silent drop (see the pipe
entries in `docs/reference/subsystems/locking.md`'s "Correctness rules learned
the hard way"). `ProcessChannel` chose drop-oldest instead, which is defensible
for an interactive console scrollback and wrong for a byte-faithful exec
stream.

## 4. Root cause B (sshd): unconditional `\n` → `\r\n` on the live path only

`bridge_process` (`userspace/sshd/src/protocol.rs`, the "Output from process to
SSH" step) translates every bare `\n` to `\r\n` before `send_channel_data` — a
PTY-rendering nicety ("so a client PTY renders lines without stair-stepping").
Two problems:

- It is applied to **exec sessions with no PTY** too. `ssh host cmd` output is
  a byte stream, not a terminal; the translation corrupts any binary or
  digest-checked output.
- It is applied **inconsistently**: the after-exit drain loop a few lines above
  sends the drained buffer **raw**, with no translation. That is why only
  9,362 of 74,898 newlines in the 512 KiB control arrived as `\r\n` — the
  chunks the bridge happened to read while the child was still alive got
  translated; everything recovered by the exit drain did not. Which newlines
  get mangled is therefore a scheduling accident.

## 5. Fix sketch (original plan, superseded by §7)

1. **Kernel**: give `ProcessChannel::write_output` real backpressure for
   channel-fed children — short-write/park the writer at the cap and wake on
   `try_read` drain, mirroring `pipe_write`/`pipe_read`'s bounded+blocking
   contract (and its lessons: raise signals and run wakes outside the lock,
   wake on *every* transition a sleeper can only observe by re-polling).
   Interactive console channels can keep drop-oldest if that behaviour is
   wanted there — but not `ChildStdout` bridges. Kernel change ⇒ boot
   self-test in `src/process_tests.rs` (writer faster than reader across the
   1 MiB cap; assert byte-exact delivery).
2. **sshd**: gate the `\n` → `\r\n` translation on the session actually having
   requested a PTY, and make the live loop and the exit drain agree (both
   translated for PTY, both raw for exec).
3. Rebuild sshd (`userspace/build.sh --sshd-only`) and repopulate the devbox
   image — the image tested here also predates the exit-status fix, so a
   refresh is due regardless.

## 6. Verify

On a boot with the fix (`INSTANCE=60`, plain `cargo run --release` in this
repo's default single-core build maps the **userspace** sshd to guest port 23
→ host 8323; the in-kernel SSH server separately owns guest port 22 → host
8222 and is a different server entirely — don't test root cause B against it):

```python
import subprocess, hashlib
def ssh(cmd, binary=False):
    r = subprocess.run(["ssh","-o","StrictHostKeyChecking=no","-p","8323",
                        "root@localhost",cmd], capture_output=True, timeout=120)
    return r.stdout if binary else r.stdout.decode().strip()

ssh("sh -c 'seq -w 1 999999 > /tmp/big.txt; md5sum /tmp/big.txt'")
data = ssh("cat /tmp/big.txt", binary=True)
# Expect: len(data) == 6999993 and md5 equal to the in-VM value, byte-for-byte,
# with NO \r\n normalization needed. Run 3×: sizes must not vary.
```

Confirmed 2026-08-01: 3/3 runs byte-exact (`de18d373283fb57966a3b69e3e8f9698`,
6,999,993 bytes each), plus the 512 KiB newline-mangling control from §2 with
no `\r\n` inserted. Needs `--full-busybox` populated on the test image — a
bare `--bin-only` disk is missing the `seq`/`md5sum`/`wc` applets and silently
falls back to a different shell's error text, which looks like an unrelated
failure if you don't know to check `busybox --list` first.

## 7. What shipped

1. **Kernel** (`crates/akuma-exec/src/process/channel.rs`): `ProcessChannel`
   gained `write_bounded` (accepts up to `MAX_BUFFER_SIZE`, returns the count
   accepted — `0` means full, never drops buffered bytes) and
   `check_set_writer` (atomically re-checks for room and registers the caller
   as a blocked writer under the same `buffer` lock, closing the TOCTOU a
   split check-then-register would have against a concurrent drain — mirrors
   `pipe_check_set_writer`). `write` (drop-oldest) is unchanged and still used
   for terminal-backed channels. `sys_write`'s `Stdout`/`Stderr` arm
   (`src/syscall/fs.rs`) now branches on `ProcessChannel::is_terminal()`:
   terminal channels keep calling `write`; exec-channel (non-PTY) children
   loop `write_bounded` → `check_set_writer` → `schedule_blocking`, exactly
   the pipe-write blocking idiom already used for `PipeWrite`/`UnixSocket`.
   Regression test: `test_process_channel_write_bounded_backpressure` in
   `src/process_tests.rs`.
2. **A second, previously-latent deadlock, found while verifying #1**:
   `ProcessChannel` keeps `buffer` and `pollers` as two *independent*
   spinlocks (unlike `KernelPipe`, which protects both under one lock).
   Every pre-existing `pollers` access (`add_poller`, the wake loops in
   `write`/`write_stdin`/`set_exited`, `is_poller_registered`) ran with IRQs
   *enabled*. `check_set_writer` needed `buffer` and `pollers` locked
   *together* (nested, under `with_irqs_disabled`) to close its TOCTOU. That
   made it the first caller ever to take `pollers` with IRQs disabled — and
   the two disciplines don't mix: if a thread holding `pollers` (IRQs
   enabled) gets preempted by a timer tick, and the preempting thread then
   spins on `pollers` with IRQs *disabled* (inside `check_set_writer`), the
   spinner can never itself be preempted back off — so the original holder
   never resumes to release the lock. Permanent freeze, no panic, and the
   timer tick itself stops firing (IRQs are off), so even the periodic debug
   heartbeat goes silent. Reproduced empirically within seconds of real
   throughput (`cat` on a 2 MiB file over the exec channel, sshd draining
   1 KiB at a time — exactly the workload this fix targets). Fixed by making
   *every* `pollers` (and, for the same reason, `buffer`) access consistently
   `with_irqs_disabled` — see the comment on `ProcessChannel::wake_pollers`.
   **Lesson for next time a channel/queue gets a second independent lock
   added to it: audit every existing caller of the lock(s) being touched for
   IRQ-disable consistency before shipping, not after a hang.** See also
   `docs/runbooks/recover-wedged-vm.md` §"Trap: ad-hoc debug prints can
   manufacture a fake wedge" — the first two (wrong) hypotheses while chasing
   this one were both artifacts of debug instrumentation, not the real bug.
3. **sshd** (`userspace/sshd/src/protocol.rs`): `bridge_process` takes a new
   `pty: bool` parameter. `run_shell_session` (interactive `shell` request,
   always `spawn_pty`) passes `true`; `run_exec_session` (`ssh host cmd`,
   plain `spawn`) passes `false`. A new `cook_output(data, pty)` helper does
   the `\n` → `\r\n` translation only when `pty` is set, and is called
   identically from both the live read loop and the post-exit drain loop —
   closing the asymmetry where only the live loop translated.

## Background

- [`INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md`](INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md)
  — the bridge's architecture and the earlier (fixed) exit-race flavour of lost
  output; this doc is the throughput flavour.
- [`EXIT_STATUS_FIX.md`](EXIT_STATUS_FIX.md) — why the short read is also
  undetectable by exit code on pre-`e54eba9` images.
- [`LIMITATIONS.md`](LIMITATIONS.md) §1 — the single-threaded cooperative
  consumer that makes the producer/consumer gap wide.
- `docs/archive/BKL_PHASE7F_OPTOUT_LIST.md` §2.2 — the integrity pass that
  surfaced this, including the HEAD-baseline reproduction ruling out Phase 7f.
