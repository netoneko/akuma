# Ctrl-C never interrupted a foreground child (`tail -f`, and anything else)

## 1. Symptom

Over SSH (`ssh -tt`), running a foreground command that doesn't itself read
stdin — the reported repro was `tail -f index.html` under `/bin/sh` — and
pressing Ctrl-C did nothing. The command kept running forever; the shell
never got its prompt back.

```
~ # tail -f index.html
hello
^C                        # nothing happens, tail keeps running
```

`paws` (the experimental `extreme-size` shell) did not have this problem —
see §4 for why that was a red herring.

## 2. Root cause: no line discipline generates SIGINT at all

Akuma has never had a real tty line discipline. `crates/akuma-terminal`
carries the *data* for one — `TerminalState::lflag`/`cc` (`ISIG`, `cc_index::
VINTR`, RFC-standard defaults: `cc[VINTR] = 0x03`) and a `foreground_pgid`
field — but nothing ever read `ISIG`/`VINTR` to actually raise a signal.
`process_canon_input` (canonical-mode line editing) handles `VERASE`/`VKILL`/
`VEOF` but has no `VINTR` case at all; a Ctrl-C byte in canonical mode just
becomes a literal character in the line buffer.

Worse, the byte-forwarding path real sessions actually use bypasses that
struct entirely. sshd's `bridge_process` writes client keystrokes straight
into the child's stdin via `/proc/<pid>/fd/0`
(`userspace/sshd/src/protocol.rs`), which resolves in the kernel to
`write_fd_data`'s `fd_num == 0` arm (`src/vfs/proc.rs`) →
`akuma_exec::process::write_to_process_stdin` (`crates/akuma-exec/src/
process/mod.rs`) — a straight byte pump, `ISIG`/`VINTR`-unaware. Ctrl-C was
just data as far as the kernel was concerned; `tail -f` doesn't read stdin at
all when tailing a file, so the byte went nowhere.

## 3. First (wrong) fix: patch sshd, target `foreground_pgid` as a single pid

The first attempt lived entirely in `userspace/sshd/src/protocol.rs`:
extend `TIOCGPGRP` with a cross-process carve-out (mirroring the one
`TIOCSWINSZ` already had for a `ChildStdout(pid)` fd — see
`src/syscall/term.rs`), have `bridge_process` query the session's
`foreground_pgid` off `stdout_fd`, and call `kill_signal(pgid, SIGINT)` on
that single pid when a `0x03` byte arrived in `CHANNEL_DATA` for a pty
session.

**This compiled clean and looked plausible. It did not fix the live test.**
Interactively verifying it (not just `cargo build`) — the same lesson
`paws`'s own Ctrl-C fix learned the hard way (a plausible, source-reading-only
diagnosis that compiled clean and matched the symptom, but was wrong until
traced live) — is what caught it:

```
~ # tail -f /root/index.html
hello
^C
~ # tail -f /root/index.html
```
(the second `tail -f` above is the same command line echoing back — the
prompt never returned; `ps aux` from a second session still showed `tail`
alive)

The bug: `foreground_pgid` is only kept current for processes spawned
through the bespoke SPAWN syscall (`spawn_process_with_channel_ext`,
`crates/akuma-exec/src/process/spawn.rs:415`, "auto-delegate foreground to
the new process") — the high-level "spawn a whole process with a piped
stdio channel" call only native `libakuma` programs use (sshd spawning the
login shell, `paws`, `box`). A real shell (`/bin/sh` — busybox/toybox on the
devbox images) launches `tail` via ordinary POSIX `fork`+`execve`
(`sys_clone`/`sys_execve`, `src/syscall/proc.rs`), which never touches
`foreground_pgid`. It stays pinned at the shell's own pid forever. The sshd
patch faithfully delivered `SIGINT` to the shell — which the shell just
absorbed (busybox `sh` catches `SIGINT`, per `userspace/sshd/docs/
LIMITATIONS.md` §"Signals the shell handles are not signal deaths") — and
never touched `tail`.

## 4. Real fix: process-group broadcast, in the kernel

The fix does not chase a moving "current foreground pid" at all. Real POSIX
`kill(-pgid, sig)` semantics — signal *every* process sharing a process group
— are what a terminal's INTR character actually uses, and Akuma already has
everything needed to do that correctly for free:

- `fork`/`clone` already propagate `.pgid` from parent to child
  (`crates/akuma-exec/src/process/mod.rs:726`, `pgid: parent.pgid`) — a
  shell that never calls `setpgid` (confirmed: `/bin/sh: can't access tty;
  job control turned off` on the devbox images) shares one `pgid` with
  everything it execs, `tail` included.
- The pty-spawned shell's own `.pgid` is already set to its own pid at spawn
  time (`crates/akuma-exec/src/process/image.rs:296`, self-leader
  convention) — the same value `spawn.rs:415` puts in `foreground_pgid`.

So `foreground_pgid` never needs to be updated for this scenario: it already
equals both the shell's `.pgid` *and* (via inheritance) `tail`'s `.pgid`.
Broadcasting to the group reaches `tail` with no per-spawn bookkeeping.
(This is also correct Unix behaviour, not a hack: a real terminal's SIGINT
hits the whole foreground process group, shell included — an interactive
job-control shell protects itself with `SIG_IGN`, and a minimal
non-job-control shell like this one survives because its blocking call is
`wait()`/`waitpid()`, which just returns `EINTR`.)

**Changes**, all kernel-side:

1. `crates/akuma-exec/src/process/signal.rs` — factored `sys_kill`'s
   per-pid delivery logic (thread-group interrupt+pend, SIGKILL hard-kill,
   and the no-live-thread fallback) out of `src/syscall/proc.rs::sys_kill`
   into `deliver_signal(pid, sig) -> bool`, and added
   `kill_process_group(pgid, sig)`, which iterates every process with a
   matching `.pgid` and calls `deliver_signal` on each. `src/syscall/
   proc.rs::sys_kill` is now a thin wrapper over `deliver_signal`.

   Both `for_each_process` loops (the sibling-tid scan inside
   `deliver_signal`, and the pgid scan inside `kill_process_group`) use a
   fixed `[T; MAX_PROCESSES]` array with a running count, not a `Vec`:
   `for_each_process`'s callback runs with IRQs disabled, which forbids
   allocation (an existing, documented convention — see the CoW-fork
   comment at `crates/akuma-exec/src/process/mod.rs` around the "pre-reserved
   Vec" note), and there can never be more matches than `MAX_PROCESSES`
   (256) live processes.

2. `crates/akuma-exec/src/process/mod.rs::write_to_process_stdin` — the
   single chokepoint every stdin-write goes through (confirmed: its only
   caller is `write_fd_data`'s `fd_num == 0` arm) — now checks, before
   handing bytes to the channel: is this session's `channel.is_terminal()`
   true (never true for a plain non-pty `exec` pipe, so a non-interactive
   client piping arbitrary binary data through `ssh host cmd < file` cannot
   trip this even though the default `TerminalState` still has `ISIG` set),
   and is `ISIG` set in `lflag`? If both, and the data contains
   `cc[VINTR]` (0x03 by default): strip that byte out of what reaches the
   channel (real tty semantics — the INTR character is consumed by the line
   discipline, never delivered as data) and call
   `signal::kill_process_group(ts.foreground_pgid, 2 /* SIGINT */)`.

3. `userspace/sshd/src/protocol.rs::bridge_process` needed **no special
   case at all** once the kernel handles it — the earlier `TIOCGPGRP`/
   `kill_signal` patch and its supporting `libakuma::get_foreground_pgid`
   wrapper were reverted. sshd just forwards bytes, same as before; the
   kernel that already receives them (`/proc/<pid>/fd/0` write) intercepts
   the INTR byte on its own.

## 5. Verification

Interactive, over a real `ssh -tt` PTY session on the devbox-smoltcp build
(`scripts/build_devbox_smoltcp.sh` + `overlays/devbox/run-smoltcp.sh`), not
just `cargo build`/`cargo check`:

```
~ # tail -f /root/index.html
hello
^C
~ #                              <- prompt returned immediately
~ # echo MARKER_6_DONE
MARKER_6_DONE                    <- shell survived, fully responsive
```

From a **second**, independent SSH session, immediately after:

```
$ ssh ... 'ps aux | grep tail'
    9 root      0:00 {busybox} -c ps aux | grep tail
```

No `tail` process — it was actually reaped, not just detached from the
terminal.

Host unit tests: `cargo test -p akuma-exec` — 265/265, unchanged (the
`deliver_signal`/`kill_process_group` refactor carries no behavior change
for the existing single-pid `kill(2)` path, only the new group-broadcast
call site).

## Background

- `docs/reference/subsystems/ssh.md` — "Terminal handling" describes the
  `TerminalState`/raw-vs-canonical machinery this fix finally wires up for
  `ISIG`.
- `userspace/paws/src/main.rs::stream_output` — `paws`'s own Ctrl-C handling
  predates this fix and works by an entirely different, narrower mechanism:
  it's the process reading its child's stdout in a loop, so it can poll its
  *own* stdin for a literal `0x03` and call `kill_signal(pid, SIGINT)`
  itself. That only ever worked because `paws` is both the shell and the
  thing driving the poll loop; it doesn't generalize to any other program
  reading stdin (e.g. a real shell blocked in `waitpid()`, not polling
  input at all while its child runs), which is exactly why `tail -f` under
  `/bin/sh` never worked even after `paws`'s own Ctrl-C bug (a blocking
  `read_fd(stdout_fd)` starving its poll loop, fixed 2026-08-12) was fixed.
- `userspace/sshd/docs/LIMITATIONS.md` — "Signals the shell handles are not
  signal deaths" explains why busybox `sh` receiving `SIGINT` directly
  (the first, wrong fix) looked like nothing happened rather than an error.
