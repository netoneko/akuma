# Terminal Syscalls for Akuma Userspace

This document outlines proposed new syscalls for the Akuma kernel to enable rich interactive Terminal User Interface (TUI) applications, such as `meow` using `ratatui`, within the userspace environment. These syscalls aim to provide the necessary low-level control over the terminal that is currently missing from `libakuma`.

## Motivation

Existing `libakuma` provides basic `read` and `write` access to `STDIN`/`STDOUT`. However, modern TUI frameworks like `ratatui` require more granular control, including:
*   Switching the terminal to raw mode for direct key event capture.
*   Precise cursor positioning and visibility control.
*   Efficient screen clearing.
*   Non-blocking input event polling.

Without these capabilities, interactive TUI applications are not feasible in the Akuma userspace.

## Proposed Syscalls

The following syscalls are proposed, inspired by Linux `ioctl` and `termios` functionalities, but simplified for the Akuma kernel.

### 1. `SYS_SET_TERMINAL_ATTRIBUTES` (New Syscall Number: 307)

*   **Description**: Sets terminal control attributes (e.g., raw mode, canonical mode, echo). This is crucial for controlling how terminal input is processed.
*   **Linux Compatibility**: Analogous to `tcsetattr(3)` with `ICANON`, `ECHO`, `ISIG`, etc., flags from `<termios.h>`.
*   **Arguments**:
    *   `fd`: File descriptor of the terminal (typically `STDIN` or `STDOUT`).
    *   `action`: An integer indicating when to apply the change (e.g., `TCSAFLUSH` for `tcsetattr`). For simplicity, we can start with immediate application.
    *   `mode_flags`: A bitmask of flags to control terminal behavior.
        *   `0x01` (RAW_MODE_ENABLE): Enable raw mode (disable canonical, echo, ISIG).
        *   `0x02` (RAW_MODE_DISABLE): Disable raw mode (restore canonical, echo, ISIG).
        *   Additional flags could be added for finer control (e.g., `ECHO_ENABLE`, `ECHO_DISABLE`).
*   **Return**: `0` on success, negative errno on failure.

### 2. `SYS_GET_TERMINAL_ATTRIBUTES` (New Syscall Number: 308)

*   **Description**: Retrieves the current terminal control attributes. Useful for saving the terminal state before changing it and restoring it afterwards.
*   **Linux Compatibility**: Analogous to `tcgetattr(3)`.
*   **Arguments**:
    *   `fd`: File descriptor of the terminal.
    *   `attr_ptr`: Pointer to a userspace buffer where the current terminal attributes (e.g., `mode_flags`) will be written.
*   **Return**: `0` on success, negative errno on failure.

### 3. `SYS_SET_CURSOR_POSITION` (New Syscall Number: 309)

*   **Description**: Sets the cursor position on the terminal screen to `(col, row)`.
*   **Linux Compatibility**: Achieved via writing VT100 escape sequences (e.g., `\x1b[{row};{col}H`) to `STDOUT`. This syscall would encapsulate that kernel-side.
*   **Arguments**:
    *   `col`: Column (0-indexed or 1-indexed, TBD, but 0-indexed is more Rust-idiomatic).
    *   `row`: Row (0-indexed or 1-indexed, TBD).
*   **Return**: `0` on success, negative errno on failure.

### 4. `SYS_HIDE_CURSOR` (New Syscall Number: 310)

*   **Description**: Hides the terminal cursor.
*   **Linux Compatibility**: Achieved via writing VT100 escape sequence `\x1b[?25l` to `STDOUT`.
*   **Arguments**: None.
*   **Return**: `0` on success, negative errno on failure.

### 5. `SYS_SHOW_CURSOR` (New Syscall Number: 311)

*   **Description**: Shows the terminal cursor.
*   **Linux Compatibility**: Achieved via writing VT100 escape sequence `\x1b[?25h` to `STDOUT`.
*   **Arguments**: None.
*   **Return**: `0` on success, negative errno on failure.

### 6. `SYS_CLEAR_SCREEN` (New Syscall Number: 312)

*   **Description**: Clears the entire terminal screen.
*   **Linux Compatibility**: Achieved via writing VT100 escape sequence `\x1b[2J` to `STDOUT`.
*   **Arguments**: None.
*   **Return**: `0` on success, negative errno on failure.

### 7. `SYS_POLL_INPUT_EVENT` (New Syscall Number: 313)

*   **Description**: Checks for pending input events (e.g., key presses) without blocking. If an event is available, it is read and returned.
*   **Linux Compatibility**: Analogous to `poll(2)` or `select(2)` on `STDIN` combined with `read(2)`. For simplicity, this syscall could return the next available event or indicate no event.
*   **Arguments**:
    *   `timeout_ms`: Milliseconds to wait for an event. `0` for non-blocking. `usize::MAX` for blocking.
    *   `event_buf_ptr`: Pointer to a userspace buffer where the event data (e.g., key code) will be written.
    *   `buf_len`: Length of the event buffer.
*   **Return**: Number of bytes read (event size) on success, `0` if no event within timeout, negative errno on failure. Event data format (e.g., raw bytes, structured event) would need to be defined.

## Implementation Considerations for Akuma Kernel

*   **Virtual Terminal Management**: The kernel's virtual terminal driver would need to be enhanced to interpret and respond to these new syscalls, sending appropriate escape sequences to the underlying console (or QEMU console).
*   **Input Handling**: For `SYS_POLL_INPUT_EVENT`, the kernel needs to manage a buffer of incoming keyboard events and provide a non-blocking mechanism to read them.
*   **`termios`-like State**: The kernel would need to maintain per-terminal state (e.g., `mode_flags`) for each userspace process interacting with the terminal.

These syscalls would provide the foundational elements for building sophisticated TUI applications in the Akuma userspace.
