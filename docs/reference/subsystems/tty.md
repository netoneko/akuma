# The terminal stack (`akuma-tty`)

**Stability: B** — extracted 2026-10-19; the same day the two line-discipline engines became one
(`ld::LineDiscipline`), which changed console/ssh-session behaviour on purpose (§ What changed).

`crates/akuma-tty` is pure, `#![forbid(unsafe_code)]`, has **no dependencies** and nothing in it
locks. The kernel keeps the acting half: `akuma-syscalls-glue/src/{term,pty}.rs` (ioctls, user
copies, wakers, the table lock), `akuma-exec`'s `ProcessChannel`, `amd64/src/console.rs`.

| module | what | who calls it |
|---|---|---|
| `termios` | flag bits, `c_cc` indices, ioctl numbers, `Termios`/`Winsize` wire layouts, `Transition`, `Termios::{initial, console_default}` | everyone |
| `ld` | `LineDiscipline`: **the** N_TTY engine — buffers, editing, echo, `ISIG`, `VMIN`/`VTIME`; termios is a parameter, never stored |
| `session` | `TerminalState`: a session's termios words, winsize, foreground group, raw-mode request, and a lazily allocated `LineDiscipline` (shared by every process of a session behind `Arc<Spinlock<_>>`) | glue `read(stdin)`, `TCGETS/TCSETS`, `SET_TERMINAL_ATTRIBUTES`; `akuma-exec::write_to_process_stdin`; amd64 console |
| `pty` | `PtyPair`: one Unix98 master/slave pair = a `LineDiscipline` + refcounts, hangup rules, waiters | glue `pty.rs` |

## Who owns each mode bit

| bit | session terminal | pty slave |
|---|---|---|
| **raw vs canonical** (`ICANON`) | `TerminalState::lflag`, read via `is_canonical()` | `PtyPair::termios` (private), `canonical()` |
| written by | `TerminalState::set_termios` (`TCSETS*`), `TerminalState::apply_mode_request` (`SET_TERMINAL_ATTRIBUTES`) | `PtyPair::set_termios` (`TCSETS*` and `set_raw_mode`) |
| edge reported as | `Transition` (`Same` / `Raw` / `Cooked`) | `set_termios` is `()` — glue's `set_raw_mode` builds the termios via `make_raw`/`make_cooked` |
| `ISIG` / `VINTR` | `TerminalState::intr_char()` (read by `write_to_process_stdin`, which kills the group) | `PtyPair::receive` raises `Signals` |
| `VMIN`/`VTIME` | `TerminalState::raw_vmin0()` for the `VMIN==0` read; glue keeps the clock | `PtyPair::slave_read` |
| winsize / fg pgid | `term_width/height`, `foreground_pgid` fields | `PtyPair::{winsize, fg_pgid}` |

Gone: `ProcessChannel::is_raw_mode` (55bbb8c0), the amd64 console's own copy of the flag, the
public `enter_raw_mode`/`exit_raw_mode` (now private behind `apply_mode_request`), the nested
`Spinlock` around `input_waker` (plain field; `set_input_waker`/`take_input_waker`), and three
hand-written spellings of the 36-byte `TCGETS` image.

**"Who put a raw terminal back in canonical mode?"** `grep -rn 'Transition::Cooked\|is_cooked'`.
Today that is three reactions, each calling `term::note_cooked` (the budgeted `[tty] raw->canonical
by pid=…` trace): glue `TCSETS`, glue `set_terminal_attributes`, and the amd64 console `TCSETS`.

Rule: **never write `ts.lflag`/`iflag`/`oflag` from a handler.** The fields are still `pub`
(readers are everywhere and tests set them), so this is convention; tests in `session.rs` pin
the funnel. Initial values are `Termios::console_default()` (amd64 console; session default
differs in `cflag`/`lflag` extras and `VSUSP`) and `Termios::initial()` (pty; adds `IUTF8`).

## What changed when the engines merged (2026-10-19)

`session` used to carry its own canonical editor; it now drives `ld::LineDiscipline`, the engine
`PtyPair` always used. Observable consequences for console and ssh-session terminals, all
Linux-correct and none a regression in the boot suites:

1. **One line per `read`** (a big-buffer read used to return every completed line at once).
2. `^X` echoes as `^X` (`ECHOCTL`); `^H` (0x08) is data unless it is `VERASE` (it used to be
   hard-wired as erase); word-erase `^W`, literal-next `^V`, `VEOL`/`VEOL2`, `ECHOK`/`ECHOKE`
   kill rub-out, `IUTF8` multi-byte erase, `IGNCR`/`INLCR`/`ISTRIP` all work.
3. `_POSIX_VDISABLE` (0) is honoured for every control character (`VERASE = 0` used to match NUL).
4. `^\` raises `SIGQUIT` and `^Z` `SIGTSTP` on the foreground group (delivered by glue after the
   terminal-state guard is dropped), and a signal key flushes what was typed before it unless
   `NOFLSH`. `^C` is still taken one layer up, at write time (`intr_char`), because a program that
   is not reading must still be interruptible. Akuma has no job-control stop: `SIGTSTP` is a no-op.
5. **Default termios is `Termios::initial()` on both kernels** (it was the amd64 literals
   `0x500/5/0xBF/0x8A3B` there and `cflag 0`, no `ECHOCTL`/`ECHOKE`/`IEXTEN`, no `VSUSP` on
   AArch64). `TCGETS` now reports `iflag 0x4500` (adds `IUTF8`) and the full control-character set.
6. `TCSETSF` / `SET_TERMINAL_ATTRIBUTES` action 2 discard the discipline's unread input too (they
   used to flush only the channel's raw bytes).
7. The raw read path is unchanged: raw bytes bypass the engine (ICRNL via `map_cr_to_nl`, echo via
   `echo_noncanon`).

Cost: a `LineDiscipline` is ~28 KiB (two rings + a line), allocated by the first canonical
`ingest` of a terminal and never for one nobody reads canonically (a `TerminalState` is per
process). `ingest` returns `NoMemory` if that allocation fails (`read` -> `ENOMEM`).

## What still differs between a session and a pty

- **When the engine runs**: a pty applies it on every master write (eager, signals raised at
  once); a session applies it on `read` (lazy), because input reaches a session as raw bytes in
  `ProcessChannel`. Making sessions eager is the next step (it would let `^C`/`^\` share one path
  and remove `intr_char`) and touches `write_to_process_stdin`, poll readiness and the amd64
  console pump.
- **Mode flips with queued input**: `PtyPair::set_termios` moves a half-edited line to the raw
  queue (and back); `TerminalState::set_termios` leaves the edit buffer alone (pinned:
  `mode_change_keeps_a_half_typed_line`) because raw bytes live in the channel, not the engine.
- **`^V^C`**: the write-time `^C` stripper (`intr_char`) cannot see a preceding `^V`, so literal-next
  before the interrupt character loses the `^C` (the `^V` is still consumed). Goes away when sessions
  become eager.
- **Output**: `translate_output` checks only `ONLCR` (pinned: `session_output_translation_ignores_opost`).
- **Readiness** for a session stdin fd is decided in glue/poll from the channel, for a pty by
  `slave_poll`.
- **Sharing**: one `TerminalState` per session (a helper's `TCSETS` re-cooks a TUI's terminal);
  one pair per pty.
- **`^C` and job control (fixed 2026-10-19)**: `^C` at `sleep 100` run from `sh -i` over ssh did
  nothing on either architecture (`[ISIG] … members=0`): `kill_process_group` always spared the
  group leader, which is right while the foreground group is the login shell's own, but a
  job-control shell makes each job its own group and `TIOCSPGRP`s to it, so the leader *is* the job.
  `TerminalState::session_pgid` (set with `set_session_leader`) remembers the shell's group;
  `spares_leader()` is true only while the foreground group is that one, and
  `akuma_exec::process::kill_foreground_group` includes the leader otherwise. Live-tested on both
  kernels: `sleep` dies (`$? = 130`), `^C` at a nested shell's prompt does not kill it, the outer
  shell survives. `PtyPair`'s signal path never had the exclusion.

## Allocation audit

`ingest` echoes through a 256-byte stack buffer and a closure: no `Vec` per read (the old
`process_canon_input` returned one). The 28 KiB engine is the one allocation, lazy and fallible.
`set_termios`/`apply_mode_request`/`intr_char`/`raw_vmin0`/`Termios` conversions work on the
stack. The `TCGETS` arm no longer holds the `TerminalState` lock across the user copy.
`read(stdin)` still allocates `count` bytes per call (`kernel_buf`) — unchanged, and now capped
at `INGEST_CHUNK` for what it takes from the channel in canonical mode.

## Verify

```sh
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2) -p akuma-tty    # 99 tests
cargo check; cargo check -p akuma-amd64 --target x86_64-unknown-none
python3 scripts/check_unsafe_budget.py
```

## Background

[`archive/AKUMA_TTY_EXTRACTION.md`](../../archive/AKUMA_TTY_EXTRACTION.md),
[`archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`](../../archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md),
[`archive/TTY_SHENANIGANS.md`](../../archive/TTY_SHENANIGANS.md),
[`archive/PIPE_TTY_FIX.md`](../../archive/PIPE_TTY_FIX.md),
[`archive/TERMINAL_SYSCALLS.md`](../../archive/TERMINAL_SYSCALLS.md).
