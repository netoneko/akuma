# Rich Terminal Interface over SSH

This document outlines the implementation of a sophisticated, low-level terminal interface within the Akuma kernel, exposed to userspace applications over SSH. The primary goal is to enable the creation of rich, interactive Terminal User Interface (TUI) applications, with the flagship target being the `meow` editor, envisioning it as a modern, pane-based coding tool.

## 1. Project Goal

The core objective was to move beyond simple line-based interaction and provide the necessary kernel and userspace primitives for full-screen TUI applications. This infrastructure allows a process to take control of the terminal, enabling raw keystroke processing, direct cursor manipulation, and screen rendering, which are essential for applications like text editors, system monitors, and other interactive tools.

## 2. Key Features Implemented

To achieve this, several interconnected features were built across the kernel, shell, and userspace libraries.

### New Terminal Syscalls (307-313)

A suite of new syscalls was introduced to give userspace processes fine-grained control over their terminal session:

- **`SYS_SET_TERMINAL_ATTRIBUTES` (307):** Sets terminal modes, most notably enabling or disabling "raw mode".
- **`SYS_GET_TERMINAL_ATTRIBUTES` (308):** Retrieves the current terminal mode flags.
- **`SYS_SET_CURSOR_POSITION` (309):** Moves the cursor to a specific `(row, col)` coordinate.
- **`SYS_HIDE_CURSOR` (310):** Makes the terminal cursor invisible.
- **`SYS_SHOW_CURSOR` (311):** Makes the terminal cursor visible.
- **`SYS_CLEAR_SCREEN` (312):** Clears the entire terminal display.
- **`SYS_POLL_INPUT_EVENT` (313):** Allows a process to perform blocking, non-blocking, or timed reads for input events (keystrokes).

### Raw Mode vs. Cooked Mode

The system now supports two distinct terminal modes for processes:

- **Cooked Mode (Default):** The shell handles line editing, echoing input back to the user, and processing control characters like backspace. Input is sent to the process only when the user presses Enter.
- **Raw Mode:** All input from the SSH channel is forwarded directly and unmodified to the process's `stdin` buffer. This allows the application to process every keystroke (e.g., arrow keys, Ctrl+C) individually. This is enabled via `SYS_SET_TERMINAL_ATTRIBUTES`.

### Asynchronous, Blocking I/O

A crucial feature is the ability for a process to block efficiently while waiting for user input. `SYS_POLL_INPUT_EVENT` was designed to avoid busy-waiting, which would waste CPU cycles and block other threads. It integrates with the kernel's scheduler and waker mechanism:

1.  A process calls `poll_input_event` with a timeout.
2.  The syscall registers a `Waker` for the current thread with the process's terminal state.
3.  It then calls `threading::schedule_blocking()`, which marks the thread as `WAITING` and yields the CPU.
4.  When new data arrives on the SSH channel, the SSH server writes to the process's `stdin` and calls the registered `Waker`.
5.  The waker marks the thread as `READY`, and the scheduler eventually runs it again, at which point `poll_input_event` can return the new input to the user process.

## 3. Implementation Details & Challenges

### The Great Preemption Deadlock

The most significant challenge was a system-wide hang that triggered the preemption watchdog.

- **Symptom:** The `termtest` program would block waiting for input, and the watchdog would report that the process's thread had preemption disabled for a critical duration (e.g., >5 seconds).
- **Root Cause:** The `SshServer` task, which runs on a dedicated thread, disables preemption around its main poll loop (`future.as_mut().poll(...)`). This is necessary to protect internal `RefCell` structures within the `embassy-net` TCP/IP stack. When `termtest` (running within this poll) made the `sys_poll_input_event` call, it eventually called `schedule_blocking()`. The `schedule_blocking` function would enter a `wfi` (Wait For Interrupt) loop. Because preemption was disabled by its caller (`SshServer`), the 10ms timer interrupt would fire but the scheduler would refuse to switch context. The thread was stuck in a CPU-hogging `wfi` loop, unable to be scheduled out.
- **Solution:** The `schedule_blocking` function was enhanced to be preemption-aware. It now saves the current preemption state, re-enables preemption for the duration of the `wfi` loop, and restores the original state upon waking. This allows the scheduler to correctly preempt a blocking thread even if its caller had disabled preemption.

```rust
// In src/threading.rs

pub fn schedule_blocking(wake_time_us: u64) {
    // ...
    let was_disabled = is_preemption_disabled();
    if was_disabled {
        // Temporarily enable preemption for the wfi loop
        enable_preemption();
    }
    
    // ... wfi loop ...

    // Restore original state
    if was_disabled {
        disable_preemption();
    }
}
```

### Other Refinements

- **Syscall Deadlock:** An initial version of `sys_poll_input_event` had a subtle deadlock caused by trying to acquire a spinlock that was already held in the same scope. This was resolved by using explicit `{}`, which ensures RAII guards are dropped correctly.
- **`libakuma` Wrappers:** All new terminal syscalls were wrapped in safe, user-friendly functions in the `libakuma` userspace library.
- **`termtest` Program:** A dedicated test program (`userspace/termtest`) was created to validate every aspect of the new interface, from raw mode switching to cursor control and blocking/non-blocking polls.

## 4. Future Goal: A Modern Interface for `meow`

With this foundational terminal infrastructure in place, the path is clear to transform `meow` from a simple file viewer into a powerful TUI-based code editor.

The plan is to use the **`ratatui`** crate to build this interface. The new syscalls provide the exact backend capabilities that `ratatui` needs to function. This will enable features common in modern coding tools, such as:

-   Split panes for viewing multiple files.
-   A file tree navigator.
-   A command palette.
-   Status bars displaying file information and editor state.
-   Interactive search and replace.

The next immediate step is to resolve the `no_std` compatibility challenges with `ratatui` and its dependencies, which will unlock the full potential of this new rich terminal interface.
