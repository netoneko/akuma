# Proposal: Phased Refactoring of Process Management Logic

This document outlines a two-phase plan to refactor the kernel's process management logic, improving modularity and maintainability.

## Problem

Currently, a significant amount of process management logic resides in a large monolithic file, `src/process.rs`. This violates the single-responsibility principle and makes the code difficult to maintain and test. Additionally, this logic rightfully belongs in the `akuma-exec` crate, which is intended to house the core execution and process abstractions.

## Phase 1: Local Refactoring (Submodule within `src`)

The first phase is a low-risk, localized refactoring to improve the immediate code structure.

**Action**:
1.  Create a new directory: `src/process/`.
2.  Create a `src/process/mod.rs` file to declare the new submodules.
3.  Split the contents of the existing `src/process.rs` into smaller, logically-grouped files within `src/process/`. For example:
    - `scheduler.rs`: Contains the thread scheduler and related logic.
    - `channel.rs`: Contains the `ProcessChannel` implementation for inter-process communication.
    - `process.rs`: Contains the `Process` struct definition and its core methods.
4.  Delete the now-obsolete `src/process.rs` file.

**Benefit**: This immediately improves code organization and makes the logic easier to navigate without introducing cross-crate dependencies or complexities.

## Phase 2: Crate Migration (Move to `akuma-exec`)

The second phase will achieve the final architectural goal of clean separation of concerns.

**Action**:
1.  Move the newly created modules from `src/process/` into `crates/akuma-exec/src/process/`.
2.  Update `crates/akuma-exec/src/lib.rs` to correctly export the new modules.
3.  Refactor the main kernel (`src/`) to use the public API now exposed by the `akuma-exec` crate. This will involve changing `use` paths and ensuring all necessary functions are made `pub`.

**Benefit**: This aligns the codebase with its intended architecture, improves reusability and testability, and reduces the complexity of the core kernel.

## Conclusion

This phased approach allows for incremental, verifiable improvements. Phase 1 provides immediate organizational benefits with minimal risk. Phase 2 achieves the long-term architectural goal in a safe and manageable way.
