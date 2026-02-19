# `paws` Shell Implementation Plan

This document outlines the roadmap for building `paws` (Process Awareness & Workspace Shell), a standalone userspace shell for Akuma.

## Phase 1: Foundation (Core Loop & Built-ins)
- [ ] **Scaffold Project:** Create `userspace/paws` with a `no_std` entry point.
- [ ] **Basic Line Reading:** Implement a simple stdin reader for command input.
- [ ] **Argument Lexing:** Use a simple lexer to handle spaces and quotes.
- [ ] **Essential Built-ins:**
    - `cd`: Using `sys_chdir`.
    - `pwd`: Using `sys_getcwd`.
    - `exit`: Standard exit.
    - `help`: Internal command listing.
    - `ls`, `cp`, `mv`, `rm`: Basic file/directory operations.
    - `find`: Recursive search.
    - `grep`: Simple pattern matching.

## Phase 2: Execution & Networking
- [ ] **Process Spawning:** Implement execution of external binaries in `/bin` via `sys_spawn`.
- [ ] **`pkg install` Built-in:**
    - Port logic from `wget`/kernel `pkg` to `paws`.
    - Support multiple package installation (sequential, continuing on failure).
    - Hardcoded target: `http://10.0.2.2:8000/`.

## Phase 3: I/O & Pipelines
- [ ] **Output Redirection:** Implement `>` and `>>` using `open`, `write`, and `close`.
- [ ] **Pipelines:** Support for the `|` operator (initially buffered via `spawn_with_stdin`).

## Phase 4: SSH Compatibility & UX
- [ ] **Terminal Control:** Use `SET_TERMINAL_ATTRIBUTES` to enable raw mode for better line editing.
- [ ] **History:** Maintain a simple in-memory command history.
- [ ] **SSH Readiness:** Ensure `paws` handles the environment correctly so it can be used as a login shell.

## Known Issues
- **No Input Echo:** In some environments (like interactive boxes), `paws` may not echo typed characters to the screen, although it correctly captures and executes commands. This is likely related to terminal mode settings or the `poll_input_event` interaction.

## Future Goal: Login Shell Integration
Once `paws` is stable, the SSH server in `src/ssh/server.rs` can be updated to:
1. Check for the existence of `/bin/paws`.
2. If found, spawn `/bin/paws` instead of entering the built-in kernel shell loop.
3. This will allow `meow` and other complex interactive apps to run seamlessly over SSH via a userspace-managed environment.
