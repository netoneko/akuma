# Proposal: `paws` - A Separate Shell Binary for Akuma Boxes

## Overview

`paws` (Process Awareness & Workspace Shell) is a minimal, Unix-like userspace shell designed specifically to run within Akuma's "Box" containers. Unlike the kernel-integrated shell used for SSH management, `paws` is a standalone binary that provides an interactive interface for isolated environments.

## Goals

1.  **Isolation Awareness:** Designed to work within the restricted filesystem and process view of a Box.
2.  **Minimal Footprint:** Small binary size with minimal dependencies, suitable for inclusion in every box.
3.  **Unix-Like UX:** Familiar syntax for pipelines, redirection, and job control.
4.  **Async-Ready:** Built to leverage Akuma's non-blocking I/O and event polling.

## Proposed Capabilities

### 1. Command Execution
- **External Binaries:** Spawning processes from `/bin` or absolute/relative paths using `sys_spawn`.
- **Argument Parsing:** Support for quoted strings and basic escape sequences.
- **Path Resolution:** Proper handling of `PATH` environment variable (or a hardcoded set of search paths like `/bin`).

### 2. Built-in Commands
To reduce the need for external binaries in minimal boxes, `paws` will include:
- `cd <dir>`: Change current working directory (using `sys_chdir`).
- `pwd`: Print current working directory (using `sys_getcwd`).
- `ls [dir]`: List directory contents (built-in to avoid dependency on an external `ls` binary).
- `ps`: List processes within the current box (using `sys_get_cpu_stats` filtered by `box_id`).
- `pkg install <package>`: Download and install a package from the host server (built-in to allow bootstrapping a box).
- `exit`: Terminate the shell session.
- `help`: Display available commands and usage.
- `echo`: Print arguments to stdout.

### 3. Pipeline & Redirection
- **Pipelines (`|`):** Support for multi-stage pipelines. Initial implementation may use `libakuma`'s `spawn_with_stdin` (buffered) with a goal to move to streaming pipes when kernel support is added.
- **Redirection (`>`, `>>`):** Redirecting stdout to files (overwrite or append).
- **Input Redirection (`<`):** (Future) Reading stdin from a file.

### 4. Interactive Interface
- **Command History:** Basic up/down arrow support for previous commands.
- **Line Editing:** Basic Backspace, Home, End, and Clear Screen support.
- **Tab Completion:** Simple completion for file paths and built-in commands.

## Recommended Crate Dependencies

`paws` should remain a `no_std` application where possible to minimize overhead.

1.  **`libakuma` (Required):** The core userspace library for syscalls.
2.  **`shlex`:** For shell-like lexing and splitting of command lines into arguments (supports `no_std`).
3.  **`arrayvec` or `smallvec`:** For stack-allocated argument lists to avoid excessive heap allocation.
4.  **`embedded-io`:** For consistent I/O traits across different streams.
5.  **`bitflags`:** For managing file open flags and terminal attributes.

## Architecture

### The Main Loop
```rust
loop {
    print_prompt();
    let line = read_line()?;
    if line.is_empty() { continue; }
    
    let plan = parse_command_line(line)?;
    execute_plan(plan).await;
}
```

### Execution Strategy
- **Built-ins:** Executed directly within the `paws` process.
- **External Commands:** 
    1.  Resolve binary path.
    2.  Use `libakuma::spawn` to create the child process.
    3.  If it's a pipeline, capture output and pass as `stdin` to the next stage.
    4.  Use `libakuma::waitpid` to monitor process completion.

## Implementation Plan

1.  **Phase 1: Minimal Shell:** Basic command execution and built-ins (`cd`, `pwd`, `exit`).
2.  **Phase 2: I/O Redirection:** Implementing `>` and `>>` using `open`/`write` syscalls.
3.  **Phase 3: Pipelines:** Implementing the `|` operator with buffered data transfer.
4.  **Phase 4: Rich Interactive UI:** Adding line editing and history.

## Integration with `box`

When a user runs `box open <name> paws`, the `box` utility will spawn `paws` as the primary process. The isolated environment's root filesystem should contain the `paws` binary at `/bin/paws` to ensure it can be found.

`paws` will be the default entry point for interactive boxes, providing a "shell-in-a-box" experience similar to `busybox sh`.
