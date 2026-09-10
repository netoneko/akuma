# amd64: an ssh session's terminal size and `TERM` never reach the shell

**Date:** 2026-09-10
**Status:** diagnosed, **not fixed**. Five distinct breaks, three amd64-only and
two shared with the AArch64 userspace.
**Symptom:** a full-screen program over ssh always behaves as if the terminal is
80×24, whatever the real window is, and `TERM` is empty.
**Grade:** C for the subsystem (expect surprises); the measurement below is A.

## 1. The measurement

Driven from a local pty deliberately set to a non-default size, so nothing can
be mistaken for a coincidental default:

```python
m, s = pty.openpty()
fcntl.ioctl(m, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 132, 0, 0))   # 132x50
# ...run `ssh -tt akuma 'busybox stty size; echo TERM=[$TERM]; echo COLUMNS=[$COLUMNS] LINES=[$LINES]'`
# with s as stdin/stdout/stderr, read the master
```

```
local pty is 132 cols x 50 rows
--- guest replied:
24 80
TERM=[]
COLUMNS=[] LINES=[]
```

On the bare metal, against the current image. `80×24` is not a stale value — it
is a **constant compiled into the kernel**, see §2 break 5.

## 2. Where it breaks — the size, by ioctl

This is the route that is *supposed* to work, and it is fully implemented on
both sides except for the two amd64 links in the middle.

| # | step | state |
|---|---|---|
| 1 | local `ssh -tt` sends `pty-req` carrying `132`/`50` | **works** (OpenSSH reads its own pty) |
| 2 | guest sshd parses it into `session.term_width/height` | **works** — `userspace/sshd/src/protocol.rs:657` |
| 3 | sshd calls `set_terminal_size(res.stdout_fd, w, h)`, i.e. `ioctl(fd, TIOCSWINSZ)` | **works** — `protocol.rs:203`, and its comment says exactly what it intends |
| 4 | the kernel should route that to the **child's** `TerminalState` | **BREAKS on amd64** |
| 5 | the child's `ioctl(0, TIOCGWINSZ)` should read that state back | **BREAKS on amd64** |

### Break 4 — `TIOCSWINSZ` lands on sshd's own terminal state

`akuma_syscalls_glue::term::sys_ioctl`'s `TIOCSWINSZ` arm finds the child by
matching the fd **exactly**:

```rust
let child_pid = match proc.get_fd(fd) {
    Some(FileDescriptor::ChildStdout(pid)) => Some(pid),
    _ => None,
};
let ts = match child_pid {
    Some(pid) => lookup_process_shared(pid).map(|p| p.terminal_state.clone()),
    None      => current_terminal_state(),        // <-- amd64 takes this
};
```

**amd64 has no `ChildStdout` descriptors.** Its `sys_spawn` hands the parent a
`PipeRead` from `alloc_pipe_fd`, so the match falls to `_ => None` and the size
is written into **sshd's own** `TerminalState` — a process that will never read
it. Nothing fails; `set_terminal_size` returns 0.

The arm's own doc comment explains why it was written for `ChildStdout`: on
AArch64 a `pty` spawn gives the child a *fresh* `TerminalState` and sshd cannot
update its own, so it must target the child's. That reasoning is sound and
amd64 simply does not have the descriptor it keys on.

### Break 5 — the child's `TIOCGWINSZ` is a hardcoded constant

`amd64::fd::console_ioctl` answers `TIOCGWINSZ` for any console fd with
literals:

```rust
w[0..2].copy_from_slice(&24u16.to_le_bytes()); // ws_row
w[2..4].copy_from_slice(&80u16.to_le_bytes()); // ws_col
```

It never consults a `TerminalState`. A spawned child's fd 0 is `< FIRST_FILE_FD`
so the preamble claims it before glue's arm — which *would* read
`current_terminal_state()` — ever sees it. **So break 5 alone is sufficient to
produce the symptom**, and fixing break 4 without it changes nothing.

This predates the 4b batch-4b `ioctl` fold: the fold preserved the hardcoded
answers deliberately (`AKUMA_AMD64_4B_FOLD_BATCH4B.md` § 2.1) because they are
what makes `isatty(0)` true for a pipe on this target. The constants were there
before and are unchanged.

## 3. Where it breaks — `TERM` and the env, which is a different story

The user's instinct on this one was right, and it is not an amd64 bug alone.

### Break 6 (shared) — sshd parses `TERM` and throws it away

```rust
let mut off = offset + 1;          // skip want_reply
let _term = read_string(payload, &mut off);   // <-- discarded
if let (Some(w), Some(h)) = (read_u32(...), read_u32(...)) { ... }
```

`pty-req`'s first field is the client's `TERM` string (the guest client sends
`xterm-256color` by default — its own `-t` flag). It is read only to advance the
offset. **Nothing on either kernel ever learns the client's terminal type.**

### Break 7 (shared) — `spawn_pty` has no env parameter

`libakuma::spawn_pty(path: &str, args: Option<&[&str]>)`. There is nowhere to
put `TERM`, `COLUMNS` or `LINES` even after break 6 is fixed. The underlying
`sys_spawn` ABI does have an `envp` slot; the wrapper does not expose it.

### Break 8 (amd64) — `sys_spawn` ignores `envp` outright

```rust
pub fn sys_spawn(path_ptr: u64, argv_ptr: u64, _envp: u64, stdin_ptr: u64, stdin_len: u64) -> u64
```

So **no environment reaches any spawned child on this target**, which is a
larger fact than terminal sizing and worth knowing on its own: anything reading
`PATH`, `HOME`, `TERM` or `TZ` from the environment gets nothing. That also
explains `TERM=[]` independently of breaks 6 and 7.

## 4. Which rig shows what

Only the amd64 side was measured. The AArch64 behaviour below is **read off the
code, not observed** — that kernel does not boot on this machine (HVF asserts;
TCG panics in a pre-existing self-test).

| break | amd64 | AArch64 (inferred) |
|---|---|---|
| 4 `TIOCSWINSZ` → wrong process | broken (no `ChildStdout`) | works — `pty` spawn gives one |
| 5 `TIOCGWINSZ` hardcoded | broken | works — glue reads `TerminalState` |
| 6 `TERM` discarded in `pty-req` | broken | **also broken** (shared code) |
| 7 `spawn_pty` has no env | broken | **also broken** (shared code) |
| 8 `sys_spawn` drops `envp` | broken | works |

So a size fix is amd64-only work, and a `TERM` fix is shared-userspace work that
would improve both.

## 5. Fix shape, in dependency order

1. **Break 5 first**, because it gates everything: make `console_ioctl`'s
   `TIOCGWINSZ` read `akuma_exec::process::current_terminal_state()` and fall
   back to 24×80 only when there is none. Small, and testable from ring 3 with
   the §6 recipe.
2. **Break 4**: either give amd64 a `ChildStdout` descriptor for the parent's
   read end, or widen glue's `TIOCSWINSZ` arm to accept a `PipeRead` that names
   a known child. The first is the honest one and it is **the same
   `ProcessChannel` work five other things want** —
   `AMD64_CONSOLE_NONBLOCK_READ.md` § 6 and `AKUMA_AMD64_4B_FOLD_BATCH4B.md`
   § 6 both point at it, which now makes six callers for one piece of work.
3. **Breaks 6 + 7 + 8** are one change with three edits: keep the `pty-req`
   `TERM`, give `spawn_pty` an env argument, and honour `envp` in amd64's
   `sys_spawn`. Worth doing together; `TERM` with no env plumbing is useless
   and env plumbing with no `TERM` is half a feature.

Note breaks 1-3 are *not* broken — the client and sshd already do their part.
This is entirely a kernel-plumbing and env-plumbing problem.

## 6. Verify (the recipe, for whoever fixes it)

A local pty at a deliberately odd size is the whole trick: run this before and
after, and `24 80` must become `132 50`.

```python
import os, pty, fcntl, termios, struct, subprocess, select, time
m, s = pty.openpty()
fcntl.ioctl(m, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 132, 0, 0))
p = subprocess.Popen(["ssh","-tt","-o","BatchMode=yes","akuma",
     "busybox stty size; echo TERM=[$TERM]"],
     stdin=s, stdout=s, stderr=s, close_fds=True)
os.close(s)
out=b""; end=time.time()+45
while time.time()<end and select.select([m],[],[],1)[0]:
    d=os.read(m,65536)
    if not d: break
    out+=d
    if p.poll() is not None: break
p.kill(); print(out.decode(errors="replace"))
```

**Do not test this by eye in your own terminal.** If your window happens to be
80×24 — or if the harness has no tty at all, in which case `ssh -tt` sends a
default — a completely broken path reports the right answer. That is why the
size above is 132×50.

## Background

- `userspace/sshd/src/protocol.rs` — `pty-req` parse (`:657`) and
  `set_terminal_size` call (`:203`); both already correct.
- `crates/akuma-syscalls-glue/src/term.rs` — the `TIOCSWINSZ` arm and its
  `ChildStdout` reasoning.
- `amd64/src/fd.rs` — `console_ioctl`, break 5.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH4B.md` § 2.1 — why the amd64 `ioctl`
  preamble exists and keeps its own answers.
- `docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md` § 6 — the `ProcessChannel`
  item that break 4 shares.
- `docs/archive/AMD64_SSH_CLIENT_TOFU_PROMPT_CR.md` — the same session's
  client-side prompt fix; unrelated cause, same feature.
