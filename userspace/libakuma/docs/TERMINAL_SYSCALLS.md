# Terminal Syscalls for Akuma Userspace

These syscalls give `libakuma` the low-level terminal control that interactive
TUI applications (e.g. `meow` using `ratatui`) need: raw mode, cursor
positioning/visibility, screen clearing, and non-blocking input event
polling. They shipped; this document is a reference for what exists, not a
proposal. `docs/archive/LIBAKUMA_AUDIT.md` item 15 is the note that this file
had drifted into describing a "proposed" API that had, in fact, been live for
some time.

## Motivation

`libakuma` provides basic `read`/`write` access to `STDIN`/`STDOUT`, but that
alone is not enough for `ratatui`-style TUIs, which need:
*   Switching the terminal to raw mode for direct key event capture.
*   Precise cursor positioning and visibility control.
*   Efficient screen clearing.
*   Non-blocking input event polling.

## Syscalls

Wrappers live in `userspace/libakuma/src/lib.rs`; syscall numbers in the
`syscall` module (`lib.rs:96-102`).

### 1. `SET_TERMINAL_ATTRIBUTES` (307) — `set_terminal_attributes(fd, action, mode_flags)`

*   **Description**: Sets terminal control attributes (raw mode, canonical mode, echo).
*   **Linux Compatibility**: Analogous to `tcsetattr(3)` with `ICANON`/`ECHO`/`ISIG` from `<termios.h>`.
*   **Arguments**:
    *   `fd`: File descriptor of the terminal (typically `STDIN`).
    *   `action`: Applied immediately; not currently used to select a `TCSAFLUSH`-style variant.
    *   `mode_flags`: `0x01` (`RAW_MODE_ENABLE`) disables canonical/echo/ISIG; `0x02` (`RAW_MODE_DISABLE`) restores them.
*   **Return**: `0` on success, negative errno on failure.

### 2. `GET_TERMINAL_ATTRIBUTES` (308) — `get_terminal_attributes(fd, attr_ptr)`

*   **Description**: Retrieves the current terminal control attributes — used to save state before changing it and restore it afterwards.
*   **Linux Compatibility**: Analogous to `tcgetattr(3)`.
*   **Arguments**:
    *   `fd`: File descriptor of the terminal.
    *   `attr_ptr`: Pointer to a userspace `u64` that receives the current `mode_flags`.
*   **Return**: `0` on success, negative errno on failure.

### 3. `SET_CURSOR_POSITION` (309) — `set_cursor_position(col, row)`

*   **Description**: Sets the cursor position on the terminal screen.
*   **Linux Compatibility**: The kernel writes a VT100 escape sequence (`\x1b[{row+1};{col+1}H`) to the process channel.
*   **Arguments**: `col`, `row` — both **0-indexed**; the kernel adds 1 before emitting the (1-indexed) escape sequence.
*   **Return**: `0` on success, negative errno on failure.

### 4. `HIDE_CURSOR` (310) — `hide_cursor()`

Writes `\x1b[?25l`. No arguments. Returns `0` on success, negative errno on failure.

### 5. `SHOW_CURSOR` (311) — `show_cursor()`

Writes `\x1b[?25h`. No arguments. Returns `0` on success, negative errno on failure.

### 6. `CLEAR_SCREEN` (312) — `clear_screen()`

Writes `\x1b[2J`. No arguments. Returns `0` on success, negative errno on failure.

### 7. `POLL_INPUT_EVENT` (313) — `poll_input_event(timeout_ms, event_buf)`

*   **Description**: Checks for pending input events (key presses) without blocking indefinitely.
*   **Linux Compatibility**: Analogous to `poll(2)`/`select(2)` on `STDIN` combined with `read(2)`.
*   **Arguments**:
    *   `timeout_ms`: milliseconds to wait for an event (converted to microseconds internally). `0` for non-blocking, `u64::MAX` for blocking.
    *   `event_buf: &mut [u8]`: buffer to receive the raw event bytes; length implied by the slice.
*   **Return**: Number of bytes read on success, `0` if no event within the timeout, negative `isize` errno on failure.

See also `POLL_INPUT_EVENT_FIX.md` for a correctness fix in this path.
