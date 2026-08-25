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
