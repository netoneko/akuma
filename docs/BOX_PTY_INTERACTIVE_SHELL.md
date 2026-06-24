# Interactive PTY for the SSH-into-box shell

## Summary

SSHing into a `stack=rump` box (`ssh -tt -p 2223 root@localhost`) lands on
`userspace/sshd` running inside the box, which spawns a busybox `/bin/sh` login
shell. Originally that shell had **no prompt, no echo, no line editing, and the
Enter key did nothing** — typing `ls` then Return produced `ls^M^M^M^M` and the
command never ran.

The kernel already had a complete, unit-tested terminal line discipline
(`crates/akuma-terminal`: ICRNL CR→NL, canonical editing, echo, ONLCR, raw-mode
enter/exit). The only thing missing was **wiring**: the box shell's channel was
spawned with `is_terminal = false`, so `src/syscall/fs.rs`'s stdin read routed
through the raw pipe path (`is_pipe = is_stdin_closed() || !is_terminal()`) and
skipped the entire discipline — including the `\r`→`\n` translation a raw-mode
SSH client (`-tt`) needs for Enter to terminate a command line.

`is_terminal` was deliberately forced `false` (see
`userspace/sshd/docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md`) to stop busybox's
line editor from hanging on an `ESC[6n` cursor query against an absent terminal.
The fix is to make that a *choice*: sshd requests a pty only for an interactive
login shell (the SSH client's `pty-req`, i.e. `ssh -tt`), exactly like a POSIX
sshd calls `openpty()` on `pty-req`.

## The wiring (Akuma's own SPAWN ABI)

`SPAWN` (syscall 301) and `SPAWN_EXT` (315) are Akuma syscalls, not the Linux
ABI, so the 6th argument was free to use as a flags word.

| Layer | Change | File |
|-------|--------|------|
| Flag bit | `SPAWN_FLAG_PTY = 1` in arg6 (kernel const kept in sync) | `userspace/libakuma/src/lib.rs`, `src/syscall/proc.rs` |
| libakuma | `spawn_pty(path, args)` wrapper passing `SPAWN_FLAG_PTY` | `userspace/libakuma/src/lib.rs` |
| sshd | interactive `run_shell_session` calls `spawn_pty()` instead of `spawn()` | `userspace/sshd/src/protocol.rs` |
| kernel | `sys_spawn` decodes arg6 → `pty: bool`, passes it to the spawn impl | `src/syscall/proc.rs` |
| akuma-exec | `spawn_process_with_channel_ext` gains `pty: bool` → `channel.set_terminal(pty)` (was hardcoded `false`); the thin wrappers default to `false` | `crates/akuma-exec/src/process/spawn.rs` |

When `pty = true` the channel reports a terminal, so `isatty()` is true and the
kernel runs its line discipline on the shell's stdin (CR→NL, canonical buffering,
echo). The child's `terminal_state` is the per-process default
(`ICANON|ECHO|ICRNL|…`) inherited from sshd, so cooked input works immediately;
if busybox switches to its own line editor via `TCSETS`, the kernel follows into
raw mode.

Why scope it to sshd and not herd/box config: the tty-ness is a property of the
sshd→shell session (does the client want an interactive terminal?), not the box.
Boxes aren't always interactive, and a per-box flag would also wrongly mark the
box's `rump_server` as a tty.

## Verification

- Boot self-test `test_spawned_child_pty_is_a_tty` (`src/process_tests.rs`,
  companion to `test_spawned_child_not_a_tty`) — **PASSED**: a `pty = true` spawn
  yields a channel whose `is_terminal()` is true.
- Live, over the NetBSD rump stack (`RUMP_NIC=1 MEMORY=1024M`, `ssh -tt -p 2223`):
  prompt, echo, and line editing work; the Enter key terminates commands.
- Networking from a busybox-spawned process in the box, over rump, works:
  `curl -H Host:ifconfig.me -L http://34.160.111.145` returns from inside the box
  shell (sysproxy-routed AF_INET → the box's `rump_server`).
- `sic` (the suckless IRC client) also ran interactively from the box shell over
  the rump stack — **caveat:** it may have issues with `^C` (interactive
  Ctrl-C / SIGINT delivery through the pty + sshd bridge to the foreground
  process is not yet confirmed clean; needs follow-up).

## Known issues (separate from the pty wiring)

### 1. Box rootfs lacks busybox applet symlinks
In `/srv/rumpbox` only `busybox`, `sh`, `rump_server`, `sshd`, `hello` exist —
no `ls`/`cat`/… symlinks (the main root gets them in `populate_disk.sh`, the box
does not). So bare `ls` is not found; `busybox ls` works via the multicall
dispatcher. Fix: stage applet symlinks into the box rootfs, or build/enable a
busybox standalone-shell.

### 2. Intermittent fork SIGSEGV — child resumes at PC=0 (OPEN, pre-existing)
`busybox <applet>` (and `wget`/`curl`) intermittently SIGSEGV (typically 1–3
failures, then succeeds). This is **not** caused by the pty wiring — fork doesn't
touch `is_terminal`; it was merely exposed once interactive commands started
running. It is the same class as the forktest/sig11 investigation
(`docs/GO_FORK_EXEC_FIXES.md`, `docs/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md`,
`scripts/forktest_*`).

Evidence captured this session:
```
[FORK-DBG] step8: marking child READY
[FORK-DBG] trampoline ENTRY tid=13
[TRAMP] tid=13 alt_sp=0x0
[IA-MISS] pid=94 ppid=90 va=0x0 ...
[WILD-IA] pid=94 FAR=0x0 ELR=0x0 x0=0x0 x1=0x20120030 x2=0x10121000 ...
  x19=0x20120030 x20=0x1005634c x29=0x202ffffe60 x30=0x0
  SP_EL0=0x202ffff850 ELR=0x0 SPSR=0x20000000
[Fault] Process 94 (/bin/sh) SIGSEGV after 0.02s
```

The forked child resumes with **only ELR(pc) and x30 == 0** while every other
GPR/SP holds a valid parent value. The child reaches EL0 via
`entry_point_trampoline` → `Process::run()` → `enter_user_mode(&proc.context)`
(`crates/akuma-exec/src/process/mod.rs`), so its user PC is `proc.context.pc`.

**Disproven hypothesis (do not retry):** that the *capture* in
`threading::get_saved_user_context` read a transiently-zeroed trap frame. A guard
that IRQ-wrapped the capture and rejected a null `elr` was added and tested — it
**never fired** (`grep -c "rejecting trap frame with null elr"` = 0) yet the child
still crashed with ELR=0. So the capture returns a valid PC; `proc.context.pc` is
zeroed **after** capture, between `new_proc.context = child_ctx` and the child's
`run()`. The guard was reverted.

Note also `SPSR=0x20000000` on the faulting child, not the `spsr=0` that
`enter_user_mode` sets — worth reconciling; it may indicate the failing child
reaches EL0 through the scheduler's fake-IRQ-frame restore
(`setup_fake_irq_frame` builds the frame; `update_thread_context` patches only
x0/x1 into it) rather than the `proc.context` path, or a preemption-in-trampoline
race.

**Next step:** instrument `proc.context.pc` at the fork set-site and in the
trampoline immediately before `run()` to localize where it becomes 0, or attach
lldb to the QEMU gdbstub (`INSTANCE=1 GDB=1`, see
`docs/` lldb+gdbstub notes) with a conditional breakpoint to catch the zeroing in
the act. The crash hits within the first few `busybox ls` attempts, so it
reproduces quickly.
