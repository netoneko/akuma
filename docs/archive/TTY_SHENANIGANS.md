# TTY_SHENANIGANS.md

## busybox `less` does not actually work (piped-stdin case)

Date: 2026-02 (branch `even-more-fixes`)
Symptom discovered live, on the kernel, by the nca agent running on it — the
first kernel feature developed inside the system.

### Symptom

- `less README.md` — works. Renders the file, cursor-position size probe
  (`ESC[999;999H ESC[6n`) and all.
- `cat README.md | less` — prints its usage banner and exits **1**. No paging.
- `echo hi | { test -t 0 && echo tty || echo not-tty; }` — prints **"tty"**.
  fd 0 in that subshell is a pipe. `isatty(0)` was lying.

### What busybox less actually checks

busybox `less` with **no FILE argument** probes `isatty(0)`:

- stdin a tty → "there is nothing being piped in to page" → usage, exit 1.
- stdin a pipe → read and page the pipe.

Because `isatty(0)` returned true inside a pipeline, every `... | less`
invocation took the first branch. `less FILE` never consults stdin, which is
why it kept working and masked the bug.

### Root cause

`isatty()` is the TCGETS path of `sys_ioctl` (`src/syscall/term.rs`). Before
the fix, the "is this a tty" decision had two inputs:

1. `fd <= 2` (only fds 0-2 could be the console tty), and
2. `current_channel().is_terminal()` — the process's I/O **channel**.

Input 2 is wrong for fork+exec pipeline children. A `cat file | less` child
has its fd 0 `dup2`d to a `PipeRead` in the fd table, but still **inherits the
shell's console channel** — which is a terminal channel. The channel says
"terminal", TCGETS succeeds, `isatty(0)` is true. The fd table entry — the
ground truth for what fd 0 actually is — was never consulted.

The channel check exists for a different, real case: sshd's exec-channel
children keep fd 0 = `Stdin` but run on a non-terminal channel, and must
report not-a-tty so busybox sh runs non-interactively instead of hanging on an
`ESC[6n` cursor query (see the comment block in `sys_ioctl`). It was never
sufficient on its own for the fork+exec pipeline shape.

### Fix

`src/syscall/term.rs`, in `sys_ioctl`, before the terminal-ioctl arms: gate on
the **fd table entry** for fd 0/1/2. Only `FileDescriptor::Stdin | Stdout |
Stderr` (the channel-backed console fds) are a tty; anything dup'd over them
(`PipeRead`, `File`, socket, …) gets `ENOTTY`, matching Linux. The existing
channel-based check is kept as a second gate for the sshd exec-channel case.

### Verification

- `cargo check` clean.
- Live re-test requires a kernel rebuild + VM reboot (the fix is in the kernel
  image, not the running one). After reboot:
  - `cat README.md | less` pages the pipe (exit 0 with `-E` or `q` on stdin);
  - `test -t 0` inside a pipeline reports not-a-tty;
  - `less FILE` still works (it never touched the changed path).

### Risk / behaviour change

Programs that redirect fd 1 to a file and still call `TIOCGWINSZ`/`TCSETS` on
it now get `ENOTTY` — which is what Linux returns, so this should strictly
improve correctness, but it is a observable change: watch the next boot
acceptance run for anything that probed termios on a redirected fd and
previously "succeeded".

---

## Round 2: `less` hangs even when the pipe is paged — no `/dev/tty`

Date: 2026-02 (branch `even-more-fixes`, uncommitted working tree)

### Symptom

With the isatty fix in, `cat README.md | less` no longer prints usage — but a
pager invoked as `git log | less` (or any `... | less` where less decides to
page) hung forever. Keystrokes were never seen by less; the bytes typed while
it hung drained into the pipe.

### Root cause

Pagers read keyboard input from `/dev/tty` — the controlling terminal —
**never** from stdin, because stdin is the pipe carrying the content being
paged. Akuma had no `/dev/tty`: the `open("/dev/tty")` failed, and busybox
less fell back to reading stdin for keystrokes. Stdin was the pipe. It never
saw a key, hung forever, and the typed bytes were consumed as pipe input.

### Fix (working tree)

- `crates/akuma-exec/src/process/types.rs` — new `FileDescriptor::DevTty`
  variant. Carries no payload: its identity IS "this process's console",
  resolved per-syscall via `current_channel()`/`current_terminal_state()`, so
  a `box grab`/reattach repoint is honoured.
- `crates/akuma-vfs/src/dev.rs` — static `/dev/tty` node (major 5 minor 0,
  Linux's TTY_MAJOR/0) so `stat`/`ls` see it. The actual `open()` is
  special-cased in `sys_openat` because it must resolve the *caller's*
  channel, which a static table cannot.
- `src/syscall/fs.rs` — `sys_openat("/dev/tty")`: returns `ENODEV` when the
  caller's channel is not a terminal (Linux says `ENXIO`; this errno set has
  no ENXIO — see the header note in `akuma-primitives/src/errno.rs` — and
  ENODEV is the nearest "device not attached" answer). Otherwise allocates a
  `DevTty` fd, honouring `O_CLOEXEC`. `sys_read` routes `DevTty` through the
  exact `Stdin` path (same channel, line discipline, canonical/echo);
  `sys_write` routes it with `Stdout`/`Stderr`; `sys_fstat` reports it as a
  character device (`st_rdev` major 136, same as the console fds).
- `src/syscall/poll.rs` — `DevTty` shares fd 0's epoll readiness and waker.
- `src/vfs/proc.rs` — `/proc/<pid>/fd` link target for `DevTty` is
  `/dev/tty`.

### Also in this working tree (needs verification before commit)

- `src/syscall/term.rs` — the fd-table gate added in round 1 (the
  `match proc.get_fd(fd) { Stdin|Stdout|Stderr => {} _ => ENOTTY }` block)
  **and** the `FIONREAD` `Stdin` arm were removed. With the gate gone,
  terminal ioctls on fd 0/1/2 are again decided by `fd <= 2` plus the channel
  check alone — i.e. the original `cat file | less` usage-banner bug is
  potentially back unless something else now covers it. `FIONREAD` on stdin
  now falls to the catch-all `0`.
- `crates/akuma-vfs/src/dev.rs` — the `static_nodes_always_exist` test was
  deleted (it asserted exactly the four pre-/dev/tty names).

### Verification

- Not yet run: `cargo check`/`cargo test` on this tree, and the live re-test
  (kernel rebuild + VM reboot). After reboot, check all of:
  - `git log | less` pages and responds to `q` via /dev/tty;
  - `cat README.md | less` still pages the pipe (regression check for the
    gate removal above);
  - `test -t 0` inside a pipeline still reports not-a-tty;
  - `ls -l /dev/tty` shows the node; a non-terminal-channel process opening
    `/dev/tty` gets an error, not the command stream.

---

## Round 3: both flagged regressions were real, plus two unrelated red herrings

Date: 2026-08-25 (branch `even-more-fixes`, after round 2 landed as commits
`73d1b099`/`b19ae838`). Triggered by a live report: `hx <file>` failed with
`Error: unable to start Helix / Caused by: No such file or directory (os
error 2)` on the `devbox-smoltcp` instance, and `nca` appeared to fail the
same way. The working hypothesis going in was that the round-1/round-2 tty
work had broken file-resolution or tty-inheritance somewhere. **Neither
guess was the actual cause of the reported crashes** — but chasing them down
surfaced two real bugs the round-2 diff's own "needs verification" section
had already flagged and never checked.

### The two reported crashes were red herrings

- **`hx <file>`: "No such file or directory (os error 2)"**, reproducible
  even via a non-interactive SSH exec (no tty involved at all). Root cause
  found with `SYSCALL_DEBUG_IO_ENABLED=true` (`src/config.rs`) + a
  syscall-trace capture: Helix shells out to `tput cols`/`tput lines` during
  startup (as `terminal_size` crate's supplementary probe) and
  `execve("*/tput", …)` failed for all 6 `$PATH` candidates — `ncurses` /
  `ncurses-terminfo-base` was never installed on the devbox rootfs, so
  neither `tput` nor a terminfo database exists. **This looked like the fix
  at first (`apk add ncurses` got `hx` past the crash) but was a dead-end: it
  is not actually needed.** Once the real bug below was fixed, `hx` launches
  fine with `ncurses` *removed again* — `terminal_size`'s ioctl-on-`/dev/tty`
  probe succeeds once `/dev/tty` ioctls work at all (see below), and the
  `tput` fallback path is simply never reached. `newfstatat`/`execve`
  correctly reported ENOENT for a file that genuinely doesn't exist; the
  bug was upstream of that, in why Helix ever needed to ask `tput` in the
  first place. Explains "neither hx nor nca required ncurses before" — they
  still don't; nothing needs to be added to `overlays/devbox/bootstrap.sh`.
- **`nca` "read error: No such device (os error 19)" / crashing in both TUI
  and non-TUI mode**: reproduced ONLY when driving it through `ssh host
  "nca"` (an SSH **exec** channel request). `userspace/sshd/src/protocol.rs`'s
  `run_exec_session` always spawns with `spawn(...)` (`pty=false`), by
  design — exec-style SSH commands are piped, not ttys, on purpose. Under a
  **real interactive** session (`ssh -tt host`, then typing at the prompt —
  `run_shell_session`, `spawn_pty`, `pty=true`), `nca` launched its
  alternate-screen TUI with no error at all. The "regression" was an
  artifact of testing method (exec vs. shell channel), not a kernel bug.
  Also disproved along the way: `is_terminal`, `isatty(0)`, `tcgetattr(0)`,
  `open("/dev/tty")`, and `TIOCGWINSZ` window-size propagation (including a
  0×0-vs-real-size A/B) all work correctly end-to-end in a genuine
  interactive session — the "tty inheritance" guess doesn't hold up either.

### The two real, verified regressions (both from round 2, `b19ae838`)

Installing `ncurses` (temporarily, for investigation — see the correction
above) got `hx` past the `tput` crash, but it then hit a NEW failure:
`thread 'main' (12) panicked at .../crossterm-0.28.1/src/event/read.rs:39:30:
reader source not set`. That panic, plus the round-2 diff's own unresolved
"needs verification" note above, pointed at `src/syscall/term.rs`'s
terminal-ioctl gate:

1. **The `cat file | less` usage-banner bug (round 1's fix) is back.**
   Round 2 deleted the fd-table type check (`match proc.get_fd(fd) {
   Stdin|Stdout|Stderr => {} _ => ENOTTY }`) entirely, leaving only the
   channel-level `is_terminal()` check — which round 1's own writeup already
   established is insufficient for a fork+exec pipeline child (it inherits
   the shell's terminal *channel* even though its fd 0 is a `PipeRead`).
   Verified live: `cat file | less` printed the busybox usage banner instead
   of paging, on the round-2 kernel.
2. **`if fd > 2 { return ENOTTY; }` unconditionally rejects the new
   `/dev/tty` fd.** `DevTty` fds are allocated fresh by `sys_openat` and are
   never 0/1/2, so this numeric cutoff — added in round 1, never revisited
   when `DevTty` was introduced in round 2 — meant **every terminal ioctl
   issued directly on a `/dev/tty` fd returned `ENOTTY`**, unconditionally,
   regardless of channel state. This is exactly how `crossterm`'s Unix event
   source (and pagers generally) use `/dev/tty`: open it, then `tcgetattr`/
   `tcsetattr` *on that fd* for raw-mode control — not on fd 0. That failure
   is what left crossterm's internal event reader uninitialized, producing
   the `reader source not set` panic.

### Fix

`src/syscall/term.rs`, `sys_ioctl`: replaced the `fd > 2` cutoff with a
single fd-table type match — `Stdin | Stdout | Stderr | DevTty` pass,
anything else gets `ENOTTY` — restoring round 1's fix and extending it to
cover `DevTty`. One gate now serves both regressions; the channel-level
`is_terminal()` check below it is unchanged (still needed for the
sshd-exec-channel case that motivated it originally).

### Verification (done, live)

Both on the round-2 kernel (broken) and the round-3 kernel (fixed), via a
real interactive SSH session (`ssh -tt`, pty allocated, realistic non-zero
window size set on the client pty so `TIOCGWINSZ` carries real dimensions):

- `cat file | less` — round 2: usage banner, exit 1. Round 3: pages the
  content correctly (cursor positioning + line erase escapes visible).
- `hx <file>` (with `ncurses` installed) — round 2: `reader source not set`
  panic, `SIGABRT`. Round 3: full TUI renders — status line, mode indicator,
  the file's actual content, syntax-highlighted.
- **Decisive check**: `apk del ncurses ncurses-terminfo-base libncursesw` on
  the round-3 kernel, then `hx <file>` again — still launches fine, no
  `tput` needed. Confirms the `tput`/`ncurses` angle was purely downstream of
  the `/dev/tty` ioctl bug, not an independent fix.
- `cargo test` (host target, all crates), plus `release`/`devbox`/
  `devbox-smoltcp`/`extreme-size` rebuilds: all green, no regressions.

### Known follow-up (not fixed, lower severity)

Round 2 also deleted the `FIONREAD` arm for `Stdin` (`current_channel()
.stdin_bytes_available()`); it now falls to the generic `_ => 0` arm, so
`ioctl(FIONREAD)` on stdin/`/dev/tty` always reports zero bytes buffered
regardless of actual pending input. Lower severity than the two above (a
conservative "nothing waiting" answer, not a hang or a wrong-answer crash),
but a program that polls `FIONREAD` before a non-blocking read to decide
whether to proceed will never see it return non-zero. Not reproduced against
a concrete failure yet — flagged here so it doesn't get lost.
