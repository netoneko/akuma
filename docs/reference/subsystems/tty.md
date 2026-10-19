# The terminal stack (`akuma-tty`)

**Stability: B** — the extraction (2026-10-19) was behaviour-preserving and is pinned by host
tests; the two line-discipline engines are still separate (see § Divergences).

`crates/akuma-tty` is pure, `#![forbid(unsafe_code)]`, has **no dependencies** and nothing in it
locks. The kernel keeps the acting half: `akuma-syscalls-glue/src/{term,pty}.rs` (ioctls, user
copies, wakers, the table lock), `akuma-exec`'s `ProcessChannel`, `amd64/src/console.rs`.

| module | what | who calls it |
|---|---|---|
| `termios` | flag bits, `c_cc` indices, ioctl numbers, `Termios`/`Winsize` wire layouts, `Transition`, `Termios::{initial, console_default}` | everyone |
| `session` | `TerminalState`: the console / ssh-session discipline (shared by every process of a session behind `Arc<Spinlock<_>>`) | glue `read(stdin)`, `TCGETS/TCSETS`, `SET_TERMINAL_ATTRIBUTES`; `akuma-exec::write_to_process_stdin`; amd64 console |
| `pty` | `PtyPair`: one Unix98 master/slave pair, full N_TTY, fixed buffers, readiness + hangup rules | glue `pty.rs` |

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

## Divergences between the two engines (input to the merge)

Merging `session` into `pty` (end state: one engine) changes behaviour on the path Claude Code's
`/login` input takes, so it is a separate step with a differential test. Observable differences
as of 2026-10-19:

1. **Echo of control characters.** `pty`: `^X` under `ECHOCTL`. `session`: the raw byte.
2. **Editing keys.** `pty` has word-erase, literal-next, `VEOL`/`VEOL2`, `ECHOK`/`ECHOKE` rub-out
   logic. `session`: erase (`VERASE` **or** `0x08`, hard-coded), kill (needs `ECHOE`), EOF only.
3. **`_POSIX_VDISABLE`.** `pty` honours 0 as "off" everywhere. `session` honours it for kill/EOF
   but not erase, and `intr_char()` returns `Some(0)` (matches NUL bytes).
4. **Signals.** `pty` raises `SIGINT/SIGQUIT/SIGTSTP`, flushes unless `NOFLSH`. `session`: only
   `VINTR`→`SIGINT`, decided in `akuma-exec`, not in the discipline.
5. **CR/NL mapping.** `pty` per byte with `IGNCR/INLCR/ISTRIP`. `session`: whole-chunk `ICRNL` in
   glue before processing.
6. **Output.** `pty` honours `OPOST`+`ONLCR`/`OCRNL`. `session::translate_output` checks only
   `ONLCR`.
7. **Raw request.** `pty::make_raw` is `cfmakeraw` (clears `ISIG`, `IEXTEN`, `ECHOCTL`…) and
   `make_cooked` cannot restore (no saved copy). `session::enter_raw_mode` keeps `ISIG`, saves
   the flags and restores them exactly — except **double-enter loses them** (pinned:
   `double_enter_raw_loses_the_cooked_flags`).
8. **Mode flip with queued input.** `pty::set_termios` moves a half-edited line to the raw queue
   when leaving canonical and marks the last raw byte a line end when entering it. `session`
   leaves `canon_buffer` alone (pinned: `mode_change_keeps_a_half_typed_line`); raw bytes live in
   the channel's stdin ring, outside the crate.
9. **Storage.** `pty`: two fixed rings, backpressure, `try_new` fallible. `session`: `Vec` +
   `VecDeque` (unbounded `canon_ready`; `canon_buffer` capped at `MAX_CANON`), `echo` and
   `drain_canon_ready` allocate per call.
10. **Readiness** is in the crate for `pty` (`slave_poll`), in glue/poll for `session`.
11. **Sharing.** One `session` state per session, shared by every process on it (which is why a
    helper's `TCSETS` re-cooks a TUI's terminal); one pair per `pty`.

## Allocation audit

New code allocates nothing: `set_termios`/`apply_mode_request`/`intr_char`/`raw_vmin0`/`Termios`
conversions work on the stack. Pre-existing allocations stay in `session` (items 9). The
`TCGETS` arm no longer holds the `TerminalState` lock across the user copy.

## Verify

```sh
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2) -p akuma-tty    # 86 tests
cargo check; cargo check -p akuma-amd64 --target x86_64-unknown-none
python3 scripts/check_unsafe_budget.py
```

## Background

[`archive/AKUMA_TTY_EXTRACTION.md`](../../archive/AKUMA_TTY_EXTRACTION.md),
[`archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`](../../archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md),
[`archive/TTY_SHENANIGANS.md`](../../archive/TTY_SHENANIGANS.md),
[`archive/PIPE_TTY_FIX.md`](../../archive/PIPE_TTY_FIX.md),
[`archive/TERMINAL_SYSCALLS.md`](../../archive/TERMINAL_SYSCALLS.md).
