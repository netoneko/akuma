# akuma-tty: one crate for the terminal stack (audit §4.2)

**Status: step 1 DONE 2026-10-19 (behaviour-preserving extraction). Step 2 (merge the two
engines) OPEN.** Reference doc: `docs/reference/subsystems/tty.md`.

## Why

`POST_AMD64_CODE_AUDIT.md` §4.2: the terminal stack was smeared over `akuma-terminal`,
`akuma-pty`, `glue/term.rs`, `glue/pty.rs`, `akuma-exec` and amd64's console/fd, and "who puts a
raw terminal back in canonical mode" (`5a577983`, the Claude `/login` investigation) had three
sources of truth and no grep-able home.

## What moved

- `crates/akuma-pty` -> `crates/akuma-tty/src/pty.rs` (+ `ring.rs`, tests); `akuma-terminal` ->
  `src/session.rs`; the flag/`c_cc`/ioctl/`Termios`/`Winsize` vocabulary both spelled -> one
  `termios.rs` (octal flags; `mode_flags`/`cc_index` are re-exports). `MAX_CANON` was defined
  twice.
- `Transition { Same, Raw, Cooked }` is what every mode change returns. `TerminalState::
  {set_termios, apply_mode_request}` are the only writers of the termios words on a session
  terminal; `enter_raw_mode`/`exit_raw_mode` went private. The three open-coded
  `was_canonical`/`is_canonical()` compares (glue `TCSETS`, glue `set_terminal_attributes`, amd64
  console `TCSETS`) became `.is_cooked()`.
- Three hand-spelled 36-byte `TCGETS`/`TCSETS` images (glue, amd64 `console_ioctl`, pty) -> one
  `Termios::{to_wire, from_wire}`. The amd64 console's compiled-in defaults
  (`0x500/5/0xBF/0x8A3B`, `VSUSP`) -> `Termios::console_default()`, pinned by a test.
- `TerminalState.input_waker`: nested `Spinlock<Option<Waker>>` (inside the `Arc<Spinlock<_>>`
  every consumer already held) -> plain field with `set_input_waker`/`take_input_waker`. The
  crate has no dependencies now (`spinning_top` gone).
- `akuma-exec`'s inline ISIG test -> `TerminalState::intr_char()`; glue's `VMIN==0` raw read
  decision -> `raw_vmin0()`.
- `TCGETS` no longer holds the terminal-state lock across the user copy.

Not moved (kept in the kernel, as asked): ioctl dispatch, user copies, wakers/BKL, the pty table,
the `note_cooked` trace, `write_to_process_stdin`'s group kill, the read loop's clock.

## Characterization (written against the old API before the funnel existed)

`session.rs` pins: a half-typed line survives raw<->cooked (`mode_change_keeps_a_half_typed_line`),
a bare `\r` is data until ICRNL (`bare_cr_is_data_until_icrnl_maps_it`), double `enter_raw_mode`
loses the cooked flags (a **pinned divergence**), second `exit_raw_mode` falls back to defaults,
`enter_raw_mode` clears exactly ECHO/ICANON/ICRNL/ECHONL/OPOST and leaves ISIG, default `c_cc`
and flags. 86 tests in the crate (67 inherited + 19 new).

## Measured

| check | result |
|---|---|
| host `cargo test` (workspace) | **2292 passed, 0 failed** |
| clippy `-D warnings`: akuma-tty/exec/syscalls-glue/syscalls-poll/vfs-glue, `--release` kernel | clean (amd64 clippy has two pre-existing errors in `hda.rs`, `clock.rs`) |
| `cargo check`, `cargo check -p akuma-amd64 --target x86_64-unknown-none` | clean |
| `scripts/check_unsafe_budget.py` | ok, 434 sites; `akuma-tty` budget 0 |
| amd64 boot suite, ryzen rehearsal (`DISK=nvme KERNEL=out/k-tty-tests overlays/ryzen/qemu.sh 0 300 std`, built ON ryzen) | **804 passed, 0 failed**, `e2fsck` clean |
| AArch64 boot suite, Lima KVM, `SMP=1`, 2 GB | **300 `[Test]…PASSED`, 1 FAILED** — identical, line for line, to a clean-`HEAD` kernel booted the same way. The one failure (`test_epoll_socket_waker`, latency 16 ms vs 11 ms) predates this: `AMD64_METAL_CONTROL_RUN_2026_10_18.md` |
| AArch64 `SMP=2` | **panics** at `exceptions::init()` "ExceptionHooks not registered" right after core 1 comes up. Seen on the new kernel only; **not** confirmed on the baseline at SMP=2 — check before assuming it is pre-existing |
| image size | +1728 B on the AArch64 release kernel (5033024 vs 5031296) |

Not run: interactive behaviour (ssh pty, Ctrl-C, a raw-mode TUI) on a live VM. On AArch64 Claude
Code cannot reach `/login` at all (`AARCH64_CLAUDE_BUN_SEGV.md`); the amd64 `/login` repro
(`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`) was not re-run.

## Step 2: two engines, one end state

`session` and `pty` still differ in 11 observable ways (listed in `tty.md` § Divergences). The
plan: a differential test feeding identical input to both, route session terminals through
`PtyPair`'s engine, delete `session`'s editing code. It changes behaviour on the `/login` input
path, so it is deliberately a separate change.
