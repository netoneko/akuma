# Proposal: Trimming the Fat (Infrastructure Optimization)

## Context
This proposal outlines a strategy to reduce the maintenance burden of the Akuma OS codebase by deprecating redundant userspace utilities and refocusing development efforts on a high-stability "Alpine-style" core.

The goal is to move from a "comprehensive standalone ecosystem" to a "minimalist runtime" model, where success is defined by the stability of a core set of syscalls required by `apk` and `busybox`.

## The Strategy: Moving to a "Core-First" Model
Instead of chasing every missing syscall for every disparate utility (e.g., `git`, `curl`, `node`), we will prioritize syscalls required by the essential primitives.

### 1. The "Cut List" (Redundant Utilities)
These components are candidates for removal as they overlap with `apk`/`busybox` or are redundant to the core mission.

#### **Category A: High Confidence (Low Risk)**
*   **`sbase/` utilities:** Any `sbase` binaries that are simple wrappers for functionality already provided by `busybox` (e.'s. `ls`, `cat`, `mkdir`, `rm`, `echo`).
*   **`userspace/top/`**: Replace with `busybox`-based top.

#### **Category B: Medium Confidence (Medium Risk)**
*   **`userspace/termtest/`**: Transition these tests into the primary crate-level test suites.
*   **`userspace/stdcheck/`**: Integrate core logic into the kernel or build-system validation.

### 2. Verification Protocol
To ensure system integrity, every removal must pass a three-stage verification loop:

1.  **Functional Replacement Test**: Verify that the `busybox`/`apk` equivalent of the removed utility produces identical output and exit codes.
2.  **Dependency Audit**: Use `ldd` (or equivalent) on all remaining binaries to ensure no broken dynamic links or missing shared objects.
3.  **Regression Boot**: Execute `scripts/run.sh` to ensure the kernel boots, the filesystem mounts, and an interactive shell is reachable.

## Implementation Workflow
1.  Create a dedicated pruning branch (`prune-sbase-bloat`).
2.  Execute removals **one component at a time**.
3.  Run the Verification Protocol immediately after each removal.
4.  Commit only upon successful verification.

---
**Note:** This assessment and proposal were produced by a local instance of **Gemma 4** as part of the ongoing effort to automate Akuma's development lifecycle.
