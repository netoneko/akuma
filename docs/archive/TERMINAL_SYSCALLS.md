# Terminal Syscalls for Akuma Userspace

This document outlines proposed new syscalls for the Akuma kernel to enable rich interactive Terminal User Interface (TUI) applications, such as `meow` using `ratatui`, within the userspace environment. These syscalls aim to provide the necessary low-level control over the terminal that is currently missing from `libakuma`.

Extra note: will get_waker_for_thread allow me to add screen-like functionality to shell? do reattach <PID> or something like that and reattach it to the current ssh session?

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

This plan outlines the implementation of terminal syscalls, specifically for the kernel's SSH-based environment without real TTYs.

**Goal:** Provide TUI capabilities for userspace applications by integrating new syscalls with the kernel's SSH session management.

**Plan:**

1.  **Syscall Interface Definition:**
    *   Define new syscall numbers (307-313) and argument structures in `src/syscall.rs` for:
        *   `SYS_SET_TERMINAL_ATTRIBUTES`
        *   `SYS_GET_TERMINAL_ATTRIBUTES`
        *   `SYS_SET_CURSOR_POSITION`
        *   `SYS_HIDE_CURSOR`
        *   `SYS_SHOW_CURSOR`
        *   `SYS_CLEAR_SCREEN`
        *   `SYS_POLL_INPUT_EVENT`
    *   Define corresponding client-side wrappers in `userspace/libakuma/src/syscall.rs`.

2.  **Kernel-side Terminal State Management:**
    *   Create a `TerminalState` structure to hold per-SSH-session terminal attributes (raw mode, cursor position, visibility, input buffer).
    *   Associate each `Process` or `Thread` with its active `TerminalState` via its SSH session context.

3.  **Implement `SYS_SET_TERMINAL_ATTRIBUTES` and `SYS_GET_TERMINAL_ATTRIBUTES`:**
    *   These syscalls will manipulate the `TerminalState` for the calling process's SSH session.
    *   Enabling raw mode will adjust the SSH input processing to bypass line buffering and echo, sending raw keystrokes to the userspace process.

4.  **Implement `SYS_SET_CURSOR_POSITION`, `SYS_HIDE_CURSOR`, `SYS_SHOW_CURSOR`, `SYS_CLEAR_SCREEN`:**
    *   Translate these syscalls into writing standard VT100/ANSI escape sequences directly to the associated SSH channel's output stream.
    *   This may require modifying `src/ssh/protocol.rs` to allow kernel-initiated control sequence writes.

5.  **Implement `SYS_POLL_INPUT_EVENT`:**
    *   Modify the SSH server's input handling (`src/ssh/protocol.rs`) to capture raw client input (key presses, etc.).
    *   Buffer these raw input events in the `TerminalState` for the respective SSH session.
    *   The syscall will read from this kernel-managed input buffer.
    *   Integrate `timeout_ms` with the kernel's event waiting and scheduler to provide blocking/non-blocking input polling.

6.  **Integration with SSH Session:**
    *   Ensure syscall handlers can identify the calling process's associated SSH session and its `TerminalState`. This might involve passing thread/process IDs.

7.  **SSH Streaming and Threading:**
    *   The SSH multi-session threading bugs are resolved, and output streaming via SSH now works reliably. This simplifies the implementation as we no longer need to account for these issues during terminal syscall implementation.

8.  **Test Cases:**
    *   Develop comprehensive userspace tests using `libakuma` for each new terminal syscall.
    *   Verify functionality over an active SSH connection, testing raw input, cursor control, screen clearing, and event polling.
