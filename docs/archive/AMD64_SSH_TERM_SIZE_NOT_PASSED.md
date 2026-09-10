# amd64: an ssh session's terminal size and `TERM` never reach the shell

**Date:** 2026-09-10
**Status:** **FIXED** 2026-09-10, verified end to end by the §6 recipe on the
rig the bug was measured on. Diagnosed as five breaks; fixing them surfaced
**two more** that the original diagnosis could not see, because the first five
masked them (§7).
**Symptom:** a full-screen program over ssh always behaved as if the terminal is
80×24, whatever the real window was, and `TERM` was empty.
**Grade:** B for the subsystem (verify behaviour) — was C; the measurement is A.

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

On the bare metal, against the pre-fix image. `80×24` was not a stale value — it
was a **constant compiled into the kernel**, see §2 break 5.

**After the fix**, same recipe, `-M microvm` + `init=/bin/herd` on the QEMU
stand-in (`SSH_PORT=2523 INIT=/bin/herd sh amd64/run.sh`):

```
local pty is 132 cols x 50 rows
--- guest replied:
50 132
TERM=[xterm-ghostty]
```

and the interactive `shell` channel — no command, so the size is asked by a
program the **login shell forked** — reports the same, which is what makes it
also the check on break 9.

Without `-tt` the client sends no `pty-req` and the answer is still `24 80` /
`TERM=[]`. That is correct and is checked deliberately: a non-interactive
`ssh host cmd < file` must keep getting a pipe.

## 2. Where it breaks — the size, by ioctl

This is the route that is *supposed* to work, and it is fully implemented on
both sides except for the two amd64 links in the middle.

| # | step | state |
|---|---|---|
| 1 | local `ssh -tt` sends `pty-req` carrying `132`/`50` | **works** (OpenSSH reads its own pty) |
| 2 | guest sshd parses it into `session.term_width/height` | **works** — `userspace/sshd/src/protocol.rs` |
| 3 | sshd calls `set_terminal_size(res.stdout_fd, w, h)`, i.e. `ioctl(fd, TIOCSWINSZ)` | **works**, and its comment says exactly what it intends |
| 4 | the kernel should route that to the **child's** `TerminalState` | **BROKE on amd64** — fixed |
| 5 | the child's `ioctl(0, TIOCGWINSZ)` should read that state back | **BROKE on amd64** — fixed |

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
    None      => current_terminal_state(),        // <-- amd64 took this
};
```

**amd64 has no `ChildStdout` descriptors.** Its `sys_spawn` hands the parent a
`PipeRead` from `alloc_pipe_fd`, so the match fell to `_ => None` and the size
was written into **sshd's own** `TerminalState` — a process that will never read
it. Nothing failed; `set_terminal_size` returned 0.

The arm's own doc comment explains why it was written for `ChildStdout`: on
AArch64 a `pty` spawn gives the child a *fresh* `TerminalState` and sshd cannot
update its own, so it must target the child's. That reasoning is sound and
amd64 simply does not have the descriptor it keys on.

**Fix**, and it is §5's second option rather than its first: the spawn row
carries the link the descriptor cannot. `Spawn::stdout_pipe` records the pipe
the parent holds the read end of, `usermode::child_of_stdout_pipe` answers
"which child is this pipe's stdout", and `fd::child_pipe_set_winsize` is a new
arm in amd64's `sys_ioctl` preamble that uses it — reaching glue only when the
fd names no spawn, which is still "the caller's own terminal" and still right.

`Spawn::stdout_pipe` is a field 4b slice 6 deleted, and it is back for a
different job: nothing routes stdio through it and nothing frees a pipe by it
(the descriptors' refcounts still own both ends). It records identity only.
Keying on a pipe id is safe **because** of those refcounts — a pipe is destroyed
only when both end counts reach zero, so while the parent holds the read end the
id names this pipe and no later spawn can be handed it. `winsize_to_child_test`
checks both halves of that: the id names the child while the descriptor is open,
and names nobody once the child is reaped.

The `ChildStdout`-for-amd64 route stays the honest long-term answer and is still
the `ProcessChannel` work `AMD64_CONSOLE_NONBLOCK_READ.md` § 6 and
`AKUMA_AMD64_4B_FOLD_BATCH4B.md` § 6 point at. This fix does not need it and
does not block it.

### Break 5 — the child's `TIOCGWINSZ` is a hardcoded constant

`amd64::fd::console_ioctl` answered `TIOCGWINSZ` for any console fd with
literals:

```rust
w[0..2].copy_from_slice(&24u16.to_le_bytes()); // ws_row
w[2..4].copy_from_slice(&80u16.to_le_bytes()); // ws_col
```

It never consulted a `TerminalState`. A spawned child's fd 0 is `< FIRST_FILE_FD`
so the preamble claims it before glue's arm — which *would* read
`current_terminal_state()` — ever sees it. **So break 5 alone was sufficient to
produce the symptom**, and fixing break 4 without it would have changed nothing.

**Fix:** that arm reads `current_terminal_state()` and falls back to 24×80 only
when there is none (the boot task). `TerminalState::default()` is the same
24×80, so a process nothing ever told reads exactly what it read before — which
is what keeps this from being a behaviour change for every other program.

The rest of `console_ioctl` is untouched and still answers from constants,
including `TIOCSWINSZ`, which stays accepted-and-dropped: a process setting the
size of its *own* console is describing a terminal this target does not own, and
the size that matters arrives on the parent's descriptor and never reaches that
arm. The hardcoded `TCGETS` in particular is what makes `isatty(0)` true for a
pipe on this target (`AKUMA_AMD64_4B_FOLD_BATCH4B.md` § 2.1) and is deliberately
unchanged.

## 3. Where it breaks — `TERM` and the env, which is a different story

The user's instinct on this one was right, and it is not an amd64 bug alone.

### Break 6 (shared) — sshd parses `TERM` and throws it away

```rust
let mut off = offset + 1;          // skip want_reply
let _term = read_string(payload, &mut off);   // <-- discarded
if let (Some(w), Some(h)) = (read_u32(...), read_u32(...)) { ... }
```

`pty-req`'s first field is the client's `TERM` string (the guest client sends
`xterm-256color` by default — its own `-t` flag). It was read only to advance the
offset. **Nothing on either kernel ever learned the client's terminal type.**

**Fix:** kept in `session.term_type`, through `wire::sanitize_term`. That filter
is not decoration — the string arrives over the wire from a peer and ends up in
a child's environment, where ncurses uses it to build a **path** into the
terminfo database, and where an `=` would forge a second environment variable.
It is a whitelist (ASCII alphanumerics and `-_.+`), a name that fails it yields
no `TERM` at all rather than a mangled one, and it lives in `wire.rs` — sshd's
host-testable half — with four unit tests: the real names that must pass, the
hostile shapes that must not, the length cap at its exact edge, and non-UTF-8.

### Break 7 (shared) — `spawn_pty` has no env parameter

`libakuma::spawn_pty(path: &str, args: Option<&[&str]>)`. There was nowhere to
put `TERM` even after break 6 was fixed. The underlying `sys_spawn` ABI does
have an `envp` slot; the wrapper did not expose it.

**Fix:** `spawn_pty(path, args, env)`. `libakuma` had **four** spawn wrappers
(`spawn`, `spawn_pty`, `spawn_with_stdin`, `spawn_with_env`) carrying four
copies of the same argv/envp marshalling and differing only in which of `stdin`,
`env` and `flags` they passed — which is how the pty one came to be the only one
with no `envp` at all. They are now four presets over one `spawn_full`, so the
marshalling has one body and one set of lifetime obligations.

### Break 8 (amd64) — `sys_spawn` ignores `envp` outright

```rust
pub fn sys_spawn(path_ptr: u64, argv_ptr: u64, _envp: u64, stdin_ptr: u64, stdin_len: u64) -> u64
```

So **no environment reached any spawned child on this target**, which is a
larger fact than terminal sizing and worth knowing on its own: anything reading
`PATH`, `HOME`, `TERM` or `TZ` from the environment got nothing. That also
explains `TERM=[]` independently of breaks 6 and 7.

**Fix:** it reads `envp` with `user_strv(envp_ptr, loader::MAX_ENVP)` and goes
through `Image::from_elf_argv_envp`, which is the entry point `sys_execve` has
always used — same caps, same stack builder, no new code below the syscall.

## 4. Which rig shows what

The amd64 column was measured before and after. The AArch64 column is **read off
the code, not observed in this session** — the fix landed against the amd64 rig
and the AArch64 image was not re-staged to carry the new `sshd`. The shared
breaks (6, 7, 10) are covered by host unit tests and by the amd64 run, which
executes the same `userspace/` source; run §6 against an AArch64 rig with a
freshly populated disk to fill the column in.

| break | amd64 | AArch64 (inferred) |
|---|---|---|
| 4 `TIOCSWINSZ` → wrong process | was broken (no `ChildStdout`) — **fixed** | works — `pty` spawn gives one |
| 5 `TIOCGWINSZ` hardcoded | was broken — **fixed** | works — glue reads `TerminalState` |
| 6 `TERM` discarded in `pty-req` | was broken — **fixed** | was **also broken** (shared code) — fixed |
| 7 `spawn_pty` has no env | was broken — **fixed** | was **also broken** (shared code) — fixed |
| 8 `sys_spawn` drops `envp` | was broken — **fixed** | works |
| 9 `fork` child gets a fresh terminal | was broken — **fixed** | works — `Process::fork` clones the `Arc` |
| 10 the `exec` channel never sizes anything | was broken — **fixed** | was **also broken** (shared code) — fixed |

## 5. Fix shape, in dependency order

The order this was written in, and it still holds:

1. **Break 5 first**, because it gates everything: `console_ioctl`'s
   `TIOCGWINSZ` reads `current_terminal_state()`.
2. **Break 4**: the spawn row carries the parent-fd → child link (see §2).
3. **Breaks 6 + 7 + 8** are one change with three edits: keep the `pty-req`
   `TERM`, give `spawn_pty` an env argument, honour `envp` in amd64's
   `sys_spawn`. `TERM` with no env plumbing is useless and env plumbing with no
   `TERM` is half a feature.

Breaks 1-3 were never broken — the client and sshd already did their part.

## 6. Verify (the recipe)

A local pty at a deliberately odd size is the whole trick: `24 80` must be
`132 50`.

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

Run the **no-`-tt`** case too. It must still answer `24 80` / `TERM=[]`: that is
the pipe path, and a fix that "improves" it has broken `ssh host cmd < file`.

Cheaper gates that do not need a live session:

- `usermode::winsize_to_child_test` in the amd64 boot suite (10 checks) follows
  a size from a parent's descriptor to a child's `TerminalState` and back out
  through `TIOCGWINSZ`, and checks it did **not** land on the caller — the half
  a single-process check cannot tell apart. Runs on every `sh amd64/run.sh`.
- `wire::sanitize_term`'s four host tests:
  `cargo test -p sshd --lib --no-default-features --target $HOST`.

## 7. Found while fixing — two breaks the first five hid

Both are real, both were invisible until the first five were gone, and the doc's
own §6 recipe could not have passed without them.

### Break 9 (amd64) — a `fork` child gets a **fresh** `TerminalState`

`register_exec_process` built `Arc::new(Spinlock::new(TerminalState::default()))`
for every process including a `fork` child. So even with breaks 4 and 5 fixed,
the size stopped at sshd's login shell: `sh` runs `busybox stty size` by
fork+execve, and that child read a brand-new 24×80.

A `fork` child inherits its parent's terminal on every Unix and on the AArch64
kernel, where `Process::fork` clones this same `Arc` (`process/mod.rs`).
`register_exec_process` now takes the terminal to use, `sys_fork` passes
`parent.terminal_state.clone()` and `sys_spawn` passes `None` — a spawned child
getting a fresh state is the deliberate half, since that is what sshd writes the
client's window into and two concurrent sessions must not share one.

Sharing the `Arc` rather than copying the size is what a live resize needs:
sshd writes the child's state on `window-change` and the running full-screen
program reads the same cell.

### Break 10 (shared) — the `exec` channel never sized anything

`ssh -tt host 'cmd'` sends `pty-req` and *then* `exec`. `run_exec_session`
spawned a plain pipe, never called `set_terminal_size`, and passed no
environment — so **the doc's own reproduction ran down a path none of breaks
4-8 were on**. That is why the recipe measures `24 80`: the exec channel had no
terminal at all, before any kernel question was reached.

`session.pty_requested` now tracks whether a `pty-req` arrived, and the exec
path takes the pty spawn, the `TIOCSWINSZ` and the CRLF cooking when it did —
and keeps its pipe, byte for byte, when it did not.

## Background

- `userspace/sshd/src/protocol.rs` — `pty-req` parse, `session_env`,
  `run_shell_session` / `run_exec_session`.
- `userspace/sshd/src/wire.rs` — `sanitize_term` and its tests.
- `userspace/libakuma/src/lib.rs` — `spawn_full` and the four wrappers over it.
- `crates/akuma-syscalls-glue/src/term.rs` — the `TIOCSWINSZ` arm and its
  `ChildStdout` reasoning, unchanged.
- `amd64/src/fd.rs` — `console_ioctl` (break 5), `child_pipe_set_winsize`
  (break 4).
- `amd64/src/usermode.rs` — `Spawn::stdout_pipe`, `child_of_stdout_pipe`,
  `register_exec_process`'s terminal argument (break 9), `sys_spawn`'s `envp`
  (break 8), `winsize_to_child_test`.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH4B.md` § 2.1 — why the amd64 `ioctl`
  preamble exists and keeps its own answers.
- `docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md` § 6 — the `ProcessChannel`
  item break 4 would also have been solved by.
- `docs/archive/AMD64_SSH_CLIENT_TOFU_PROMPT_CR.md` — the same session's
  client-side prompt fix; unrelated cause, same feature.
