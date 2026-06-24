# sshd interactive-shell bridge: the "lost command output" drain fix

**Status:** root-caused + fixed (2026-06-24). Decision: ship **toybox** (`toysh`)
as the interactive login shell for the SSH-into-box demo (`acceptance/11`).

This documents the investigation and every fix made while getting an SSH login
shell to round-trip commands (`ssh -tt host` → type a command → see its output)
over the userspace `sshd` (`userspace/sshd`). It covers a kernel bug (the real
root cause), a userspace-sshd cleanup, and a small CLI addition.

---

## 1. Symptom

Logging into the userspace `sshd` and running a command produced **no output**:

```
$ ssh -tt -p 2323 root@localhost     # busybox /bin/sh login shell, smoltcp box 0
echo HELLO
exit
# → (empty stdout; connection closes)
```

Login, auth, shell spawn, and the output *direction* were all already proven
(a print-and-exit `/bin/hello` shell streamed its output fine). What failed was
the round-trip: type a command → see its output.

## 2. How the bridge works

`sshd` has no PTY. For an external login shell it spawns the shell as a child and
**bridges** the SSH channel to the child's pipes (`userspace/sshd/src/protocol.rs`,
`bridge_process`):

- child **stdout** → a kernel `ProcessChannel`; the parent holds a
  `FileDescriptor::ChildStdout(child_pid)` read fd (returned by `spawn`).
- SSH `CHANNEL_DATA` → written to the child's **stdin** via `/proc/<pid>/fd/0`.
- SSH `CHANNEL_EOF` → `close_child_stdin(pid)` so a shell reading a piped script
  finishes and exits.

The loop, each iteration:

1. `waitpid(pid)` — if the child exited, **drain** remaining stdout, then stop.
2. otherwise read stdout (non-blocking) and forward it;
3. read SSH input and forward it to the child's stdin.

## 3. Isolation: it's not the bridge

We pointed `--shell` at an **independent** shell — toybox's `toysh` — on a second
diagnostic port, leaving everything else identical:

| Login shell | `echo X; exit` over the bridge |
|-------------|--------------------------------|
| busybox `ash` (`/bin/sh`, :2323) | **empty** — every time (0/N) |
| toybox `toysh` (`/bin/toybox sh`, :4444) | **`X`** — single commands 5/5 |
| toybox, `echo HI; sleep 2; echo BYE; exit` | only `HI` — the **last** line (`BYE`, written right before exit) is lost |

So the bridge itself works. The discriminator is *when* the shell emits output:

- **busybox** uses buffered stdio and flushes it all at `_exit` → its output only
  ever exists in the channel at the *instant of exit* → always lost.
- **toybox** writes incrementally → output produced while it's still running
  survives; only the final write-just-before-exit is lost.
- a bare `echo X; exit` is flaky for toybox depending on whether the bridge polled
  stdout before the child exited.

That pattern — *output written immediately before exit is dropped* — points at the
exit path, not the shell.

## 4. Root cause (kernel)

The parent's `ChildStdout(child_pid)` fd resolves the channel **by pid** on every
read (`src/syscall/fs.rs` → `get_child_channel(child_pid)`), and that read path is
correct: it returns buffered bytes first, and only returns `0` (EOF) once the
buffer is empty *and* the child has exited.

But `sys_waitpid` (and `sys_wait4`, `sys_waitid`) called
`remove_child_channel(pid)` the instant it reaped the zombie
(`src/syscall/proc.rs`). The bridge checks `waitpid` **first** each iteration, so:

1. the iteration that observes the exit → kernel removes the channel from the
   `CHILD_CHANNELS` registry (its buffered stdout goes with it);
2. the bridge's post-exit drain → `read_fd(stdout_fd)` → `get_child_channel` now
   returns `None` → `EBADF`/0 → **all buffered output is lost.**

`waitpid` reaping the zombie (process table) and the lifetime of the stdout pipe
are two different concerns; tearing the pipe down at reap violated Unix pipe
semantics (buffered data must stay readable until the reader drains or closes it).

## 5. Fixes

### 5a. Kernel — keep the child channel until it is drained (the real fix)

`crates/akuma-exec/src/process/children.rs` gains `reap_child_channel(pid)`, used
by the `wait*` paths instead of `remove_child_channel`:

```rust
/// Reap on the wait* path: only drop the channel if its stdout buffer is empty;
/// otherwise keep it so the parent can still read output the child wrote right
/// before exiting. The parent's close() (or teardown) removes it once drained.
/// Race-free: the child is confirmed exited, so the buffer can only shrink.
pub fn reap_child_channel(child_pid: Pid) -> bool { /* keep if has_stdout_data() */ }
```

Applied at all six wait-reap sites (`sys_waitpid`, the two `sys_wait4` arms ×2,
`sys_waitid`). The `close()`/`execve`-cloexec removals (`sys_close`,
`sys_close_range`, `do_execve`) are unchanged — there the **reader's fd is gone**,
so removing the channel is correct.

### 5b. sshd — close the stdout fd after draining

`bridge_process` now `close(stdout_fd)`s after the loop. With 5a the channel
survives `waitpid`, so something must still free it once drained — closing the
`ChildStdout` fd is what does that. Without it, `sshd` (which handles connections
serially in one long-lived process) would leak one channel per login.

### 5c. sshd — `--shell-arg` for multicall shells

toybox/busybox are multicall binaries: the applet is chosen by argv, so the login
shell must be spawned as `argv = ["/bin/toybox", "sh"]`. Added a repeatable
`--shell-arg` flag (`userspace/sshd/src/{config,main,protocol}.rs`); herd splits
`args` on whitespace, so:

```
# bootstrap/etc/herd/enabled/sshd_diag_toybox.conf
args = --port 4444 --shell /bin/toybox --shell-arg sh
```

## 6. Tests

- **Host unit tests** — `crates/akuma-exec/src/process/children.rs`
  (`child_channel_drain_tests`): reap **keeps** a channel with buffered stdout,
  the parent can still resolve + drain it, a drained reap then **removes** it, an
  empty channel is removed immediately (no leak), and reaping an absent pid is a
  no-op. Run: `cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2) -p akuma-exec`.
- **Boot self-test** — `src/process_tests.rs::test_waitpid_reap_preserves_buffered_stdout`:
  spawns `/bin/hello`, waits for exit *without* draining, asserts the reap keeps
  the channel and the buffered output is still readable, then that a drained reap
  removes it.

## 7. Verification

After the fix, busybox `/bin/sh` over the same `:2323` bridge delivers output
(it was empty every time before):

```
$ ssh -tt -p 2323 root@localhost    # input: "echo HELLO_HOST_BUSYBOX\nexit\n"
HELLO_HOST_BUSYBOX

$ ... input: "echo A\nsleep 1\necho B\nexit\n"
A
/bin/sh: sleep: not found
B                                   # ← 'B', written right before exit, now survives
```

Test results:
- Host: `cargo test -p akuma-exec child_channel_drain` → **3/3 pass** (108 total, 0 failed).
- Boot self-test: `[PASS] test_waitpid_reap_preserves_buffered_stdout` (0 panics).
- E2E: busybox `/bin/sh` and toybox `sh` both round-trip commands over the bridge.
- `cargo clippy -p akuma-exec` clean.

## 8. Decision

Ship **toybox `toysh`** as the interactive login shell for the SSH-into-box demo.
Even before 5a it round-tripped commands reliably; with 5a, busybox works too, but
toybox is the chosen shell here. `bootstrap/bin/toybox` is built static for
aarch64-musl (`CONFIG_SH` + builtins; see the build notes below).

### toybox build notes (cross on macOS)

`make defconfig` then enable `CONFIG_SH` (+ its builtins: `EXIT`, `CD`, `SET`, …),
and work around two macOS-host issues:
- disable `CONFIG_TOYBOX_ZHELP` — its `od -Anone -vtx1 | sed 's/ /,0x/g'` help
  compressor emits malformed `0x,` constants under BSD `od`;
- force GNU linker flags: toybox picks `-dead_strip` / host `strip` from the host
  `uname`; cross-linking needs `LDOPTIMIZE='-Wl,--gc-sections -Wl,--as-needed'`
  and `STRIP=aarch64-linux-musl-strip`.

## 9. Diagnostic harness (smoltcp box 0, no rump)

Two userspace-`sshd` services on smoltcp, reachable from the host:
- `:2323` (guest :23) — busybox `/bin/sh` (`sshd_host.conf`)
- `:4444` (guest :4444) — toybox `sh` (`sshd_diag_toybox.conf`)

`disable_key_verification = true` in `/etc/sshd/sshd.conf`, so login needs no key.
Both run outside any box (no `join_box`, no `stack=rump`) to isolate the bridge
from the rump sysproxy path.
