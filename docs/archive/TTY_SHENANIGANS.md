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
