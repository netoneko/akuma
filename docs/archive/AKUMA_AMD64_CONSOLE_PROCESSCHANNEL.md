# amd64: the console becomes a `ProcessChannel`

> 2026-09-11. Piece **A** of
> `proposals/NEXT_AGENT_AMD64_PROCESSCHANNEL_AND_LIFECYCLE.md`, which is C1's
> last fold. Piece B (the lifecycle unification) is untouched and still next.
>
> **No shared crate changed.** `git status -- crates/ src/` is clean across the
> whole change, so the AArch64 kernel is byte-identical and needed no run.

## 1. What it was

Four defects in `amd64/src/fd.rs`, which said so itself in five places —
"**no process on this target has one**":

| symptom | mechanism |
|---|---|
| the console is always cooked | `read_console` called `process_canon_input` unconditionally; `console_ioctl` took `TCSETS` as a no-op and answered `TCGETS` from compiled-in literals, so a raw-mode request was recorded nowhere |
| no `EINTR` on a console read | `read_console` had no `should_interrupt_blocking_syscall` test; every other read arm does |
| `/dev/tty` is an instant EOF | `console_end` answered `None` for `FileDescriptor::DevTty` |
| glue's console readiness needs a hook | `boot.rs` registered `poll_console_state` because glue reaches a console through a `ProcessChannel` |

## 2. What was actually wrong — the producer, not the fold

The prompt framed this as a deletion: give a process a channel, drop four
preambles. The deletion is real, but it is the *second* half. Glue's
`Stdin`/`DevTty` arm is a **consumer** — it drains `read_stdin`, applies the
line discipline, echoes through `write`, and on an empty FIFO registers an
`input_waker` and parks. Every one of those already worked here. **Nothing
filled the FIFO.**

On the AArch64 kernel the producer is userspace: `sshd` writes
`/proc/<pid>/fd/0`, which lands in `write_to_process_stdin`. That kernel never
reads a console input device at all — there is no UART reader anywhere in
`src/`. This target is the other half of `TTY_SHENANIGANS.md` round 3: here the
console **is** an input device, polled destructively through `input::getb`, and
until now it was polled from inside the reading thread itself.

So the piece is `amd64/src/console.rs` (303 lines): one `ProcessChannel`, one
`TerminalState`, and a **pump** — the console's answer to `sshd`'s
`bridge_process`, one daemon carrying both directions.

- in: `input::getb` → `ch.write_stdin` → fire the `input_waker`.
- out: `ch.read` (the line discipline's echo, which glue writes to the channel's
  *stdout* FIFO) → `serial::putb`. Without this half a typed character would
  never appear and the FIFO would climb to its 1 MiB cap.

Two details that are not incidental:

- **`has_stdout_data()` guards the drain.** An empty `ProcessChannel::read`
  calls `add_poller(current_thread_id())`, so reading unconditionally would
  register the pump as a poller on every idle lap.
- **The pump yields; it does not park on a deadline.** `block_until_deadline`
  resolves at the LAPIC tick and `main.rs` starts the timer only when there is a
  network. A pump parked on a clock that is not ticking is a console that never
  delivers a keystroke. `yield_now` needs only the round-robin, and it is the
  cost profile this target already accepts — `netpoll_daemon` is a permanent
  yield loop and `read_console` was one for the whole time a shell sat at a
  prompt.

## 3. The claim that had already gone stale

`fd.rs` said the fold would answer **EOF**: glue's arm falls back to
`Process::read_stdin` when `current_channel()` is `None`, and that buffer is
never filled here.

It would not have answered EOF. It would have **parked forever**.

`current_channel()` tries `Process::channel` and then falls back to
`get_channel(current_thread_id())` — and the **exit channel** adopted
2026-09-10 (`AKUMA_AMD64_RING3_SEAM_SLICE7.md`) registers through exactly that
map, for every registered process. `ProcessChannel::new()` defaults
`is_terminal` to `true`. So on the tree as it stood, every amd64 process had a
channel that reported itself a terminal and whose stdin FIFO nothing would ever
fill.

Two consequences were already live and unnoticed:

- `open("/dev/tty")` **succeeded** (glue gates it on
  `current_channel().is_some_and(|c| c.is_terminal())`), and the read then
  parked — worse than the documented instant EOF.
- Any future fold of a `Stdin` read would have hung rather than returned 0,
  i.e. failed in the mode that says the least.

The fix keeps the two channels apart by *role*: `Process::channel` is the
console's I/O channel and is set only for a console-attached process; the exit
channel stays in the per-thread map carrying an exit status, exactly as its own
comment says.

## 4. What attaches, and to what

`usermode::register_exec_process` asks one question — is fd 0 in this process's
table a `FileDescriptor::Stdin`? — and on a yes hands over **both** the console
channel and the console's `TerminalState`.

- `init` on the serial line and its `fork` descendants: yes.
- everything `sshd` spawns: no. Its fd 0 is a `PipeRead` from `bind_stdio`, and
  glue's pipe arm serves it.

One `TerminalState` shared by every console process is not a shortcut, it is
what a tty is: one serial line, one set of termios flags, one input queue. It is
also what makes the wake land — the pump fires the `input_waker` on that cell
and glue's arm registers on `current_terminal_state()`, which *is* that cell.
Two objects there means a keystroke that wakes nobody.

The boot row needed the same treatment and is the one case that cannot use the
field: `make_test_process` builds it and its fields are not reachable
afterwards, so `boot_row_register` registers the channel through
`register_channel(tid, ..)` instead. Its line discipline is deliberately **not**
registered — that would shadow `Process::terminal_state`, which is what
`winsize_test` writes and reads back. (Found by both checks failing in turn,
one per iteration.)

## 5. What went away

| | before | after |
|---|---|---|
| `read_console` | 49 lines + 52 of doc | deleted |
| `fd::CONSOLE` + `init_console` (a second `TerminalState` for one serial line) | 25 | deleted |
| `sys_read`'s console preamble | 9 | deleted |
| `sys_lseek`'s `/dev` preamble | `dev_node_of(fd).is_some()` | `…is_some_and(\|n\| n != "tty")` — a tty is not seekable and glue already answers `ESPIPE` |
| `poll_console_state` | both spellings | unbound only; a **bound** descriptor goes to glue's arm, which registers a poller, so a parked `poll` wakes on the keystroke instead of at the next 10 ms tick |
| `console_ioctl` `TCGETS`/`TCSETS` | literals / no-op | read and write the process's `TerminalState` |

`fd.rs` is 245 lines added, 240 removed — the fold is not a net deletion,
because what replaced the preambles is the documentation of why they were there.

**`ioctl`'s preamble stays**, and the doc now says which half of its reason is
retired: the *console* has a terminal-capable channel, so `poll` works on its
own; an *sshd session's child* still has none, its stdin is a pipe, and the
`fd < FIRST_FILE_FD` fake tty is what lets `busybox sh` run interactively over
the bridge at all. That is the deferred `/proc/<pid>/fd/0` + `delegate_pid`
work, unchanged.

### The divergence that had to be carried

`console_ioctl` answered `TCGETS` from literals that do **not** match
`TerminalState::default()`: `c_cflag = B38400|CS8|CREAD` (the default is 0, i.e.
hang-up), `c_lflag`'s `ECHOCTL|ECHOKE|IEXTEN`, and `c_cc[VSUSP]` (which
`akuma_terminal::cc_index` does not even name). Reading the state directly would
have silently re-described every terminal on this target.

`console::default_terminal_state()` seeds exactly those values, and is used for
**every** process this target registers — not just the console one — so a
spawned child's fake tty keeps reporting what it reported. Verified on the metal
over ssh: `busybox stty -a` still says `speed 38400 baud` and `susp = ^Z`.

Not fixed in `akuma-terminal`, deliberately: `cc_index::VSUSP` belongs there and
adding it would change what the AArch64 kernel's `TCGETS` reports, which is a
behaviour change on a kernel this change otherwise does not touch.

## 6. Verification

The boot suite cannot check any of this — every path is interactive. Two gates
do.

**`consoletty`, as init on the serial line** (`INIT=/bin/consoletty`), which is
the distinction the probe exists for: over ssh a process's fd 0 is a `PipeRead`
and would pass against a broken kernel. Extended by 13 checks for what this
change adds — the raw-mode round trip (`cfmakeraw` → `TCSETS` → `TCGETS` reads
`ICANON`/`ECHO` clear, a raw non-blocking read is still `EAGAIN`, restore), and
`/dev/tty` (opens, `isatty`, and a non-blocking read is `EAGAIN` rather than an
instant EOF or a park).

**Typed input, A/B against a `HEAD` worktree.** QEMU's serial is `mon:stdio`, so
a delayed feed is real keyboard input:

```
python3 -c "import time,sys; time.sleep(28); sys.stdout.write('PUMPWORKS-1\n'); …" \
  | DISK=… SMP=1 INIT=/bin/busybox INITARGS=cat sh amd64/run.sh -display none
```

Both kernels print each line **twice** — once as the line discipline's echo,
once as `cat`'s output. Identical before and after, which is the point: the
bytes now travel hardware → pump → channel → glue's `Stdin` arm → `cat`, and the
echo travels back out through the channel's stdout FIFO.

(The first attempt piped the input at QEMU start and saw nothing on *either*
kernel — bytes written before the guest's UART is up are simply lost. That is a
property of the rig, not of the change, and it cost one confusing A/B.)

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 641/0 | **641/0** |
| Firecracker/KVM `SMP=4` | 619/0 | **619/0** |
| **bare metal** `SMP=4` `root=/dev/sda1` | 641/0 | **641/0** |
| `consoletty` (ring 3, as init) | 41/41 | **54/54** |
| typed input through `busybox cat` | echo + delivery | **echo + delivery**, identical |
| `busybox stty -a` over ssh, on the metal | — | `speed 38400 baud`, `susp = ^Z` |
| clippy, amd64 | clean | **clean** |
| AArch64 | — | **untouched** (`crates/`, `src/` unchanged) |

The bare-metal tally arrives torn, as the runbook warns: at `SMP=4` a
`[BKL] stuck: owner=1 waiter=2 tag=511` print lands between `641 passed,` and
`0 failed`. `dmesg | sed -n '1,/all self-tests/p' | grep -ac FAIL` → 0 is the
check that survives it. The `tag=511` storm is pre-existing and load-driven.

## 7. What is still open

- **`EINTR` on a console read** is now glue's, so it arrives the moment this
  target delivers a signal — it is no longer a gap in `fd.rs` but a gap in
  signal delivery. `AMD64_CONSOLE_NONBLOCK_READ.md` §6 item 1 said to look at
  delivery first; that ordering still holds and nothing here forced it.
- **INTR → SIGINT on the foreground group.** The pump delivers `^C` as a byte.
  The shared route that turns it into a signal is `write_to_process_stdin`'s
  ISIG handling, which needs a pid, i.e. a foreground-process notion this target
  does not have.
- **An sshd session's child still has no channel** — §5.
- A *blocking* console read from the **boot task** would park on a waker the
  pump never fires (its line discipline is its own, §4) and wait out the 1 Hz
  untimed-park backstop. No check does that and the pump is not spawned until
  `run_init`; the note is in `boot_row_register` for whoever tries.

## Background

`AMD64_CONSOLE_NONBLOCK_READ.md` §6 (the four adjacent items, item 4 already
closed), `AKUMA_AMD64_4B_FOLD_BATCH4B.md` §6 (`fd.rs`'s floor),
`AKUMA_AMD64_4B_FOLD_BATCH2A.md` § `/proc` (the deferred design),
`TTY_SHENANIGANS.md` round 3 (the two interactive-shell architectures),
`AKUMA_AMD64_RING3_SEAM_SLICE7.md` §1 (the exit channel, and the failure mode
this series keeps hitting).
