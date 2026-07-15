# In-kernel editor (neko)

The kernel's built-in text editor, "neko" — a nano-like, modeless-keybinding
terminal editor with pluggable I/O and filesystem, exposed over the same
`:2222` SSH session as the [in-kernel shell](shell.md). Source:
`src/editor/mod.rs` (kernel adapter) + `crates/akuma-editor/src/lib.rs`
(the editor itself, extracted into a host-testable crate in Mar 2026,
`bedb477`).

**Not to be confused with `neatvi`** — an unrelated, ISC-licensed C vi clone
compiled on-target via TCC and installed as userspace `/bin/vi` in the devbox
overlay (see `overlays/devbox/README.md`, `docs/archive/LOW_MEMORY_
ENVIRONMENT.md`). Despite similar naming/purpose there is no code sharing:
`neko` is a from-scratch Rust `no_std` editor; `neatvi` is third-party C.

> **Stability: A (stable).** Functionally dormant since Jan 2026
> (`text editor works when resized over ssh`) and the Mar 2026 crate
> extraction; the only touch since is a one-line `#[cfg(feature =
> "smoltcp")]` gate on a re-export (Jul 3 2026, `optional smoltcp, almost
> done`) with no behavioral change.

## Kernel adapter (`src/editor/mod.rs`)

Only 33 lines: `KernelFs` implements the crate's `EditorFs` trait
(`read_to_string`/`write_file`) by delegating to [`async_fs`](async-fs.md)
(`:17-25`); `run()` (`:28-33`) is the sole entry point, calling
`akuma_editor::run(stream, &KernelFs, filepath)`. `TermSize` is re-exported
only under `#[cfg(feature = "smoltcp")]` since it's consumed solely by the
built-in SSH editor integration — a rump-only (no-smoltcp) build doesn't
need it.

## Editor core (`crates/akuma-editor`)

- **`EditorFs` trait** (`:24-27`) — the pluggable filesystem boundary; the
  crate itself has no knowledge of VFS, `with_fs`, or any kernel type.
- **`TermSize`/`TermSizeProvider`** (`:49-73`) — terminal dimensions, pulled
  from the stream each input loop iteration so a live SSH terminal resize is
  picked up (`content_rows()` reserves 4 rows for status/message lines).
- **`EditorBuffer`** (`:80-`) — owns document content and cursor state,
  loaded via `EditorFs::read_to_string`.
- **`Editor`** (`:310-352`) — top-level state: buffer, `EditorMode`
  (`Normal`/`Message`), a status `message`, and a `running` flag that
  `run()`'s loop checks each pass.
- **`InputParser`/`InputEvent`** (`:355-`) — a stateful byte-at-a-time
  parser (`EscapeState`: `Normal`/`Escape`/`Bracket`/`Extended`) turning raw
  terminal bytes into `InputEvent`s: arrow keys, `Home`/`End`/`Delete`/
  `Backspace`/`Enter`, and nano-style chords `CtrlO`/`CtrlX`/`CtrlA`/`CtrlE`
  (`:371-389`).
- **`run()`** (`:656-708`) — the main loop: poll terminal size each pass,
  render (`render_screen`), read up to 32 bytes, feed each byte through
  `InputParser`, dispatch the resulting `InputEvent` via `handle_input`.
  Clears the screen and restores the cursor on exit.

## Background

- `docs/archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md` — the raw-mode/cursor-
  control terminal syscalls this editor's SSH integration is built on
  (originally scoped around a `meow` editor target — a separate userspace
  project, not this one).
