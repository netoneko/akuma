# Fork mmap Copy & Wait Status Encoding Fix

## Problem

Pipes in Linux binaries (e.g. `busybox.static sh`) silently fail:

```
/ # echo hello | grep e
/ #
```

No output, and the shell exits with the mysterious code 245.

## Root Cause (two bugs)

### Bug 1: Forked children crash — missing mmap regions

`fork_process` copies the parent's code, heap, and stack into the child's
address space, but **skips all mmap regions**:

```rust
// OLD — all mmap pages dropped
new_proc.mmap_regions.clear();
```

This was justified by the comment "fork is almost always followed by execve
which replaces the address space". That is true for `make`-style vfork+exec,
but **not** for busybox ash pipes: busybox forks a child and runs a built-in
applet (echo, grep, etc.) directly in the child — no `execve`. If musl libc
or busybox has any mmap-backed allocations (large malloc, internal buffers),
the child hits an unmapped page and takes a data abort (SIGSEGV).

### Bug 2: Signal deaths look like normal exits — wrong wait status encoding

When a child crashes (e.g. SIGSEGV, exit code −11), `return_to_kernel(-11)` is
called. `sys_wait4` then encodes the status as:

```rust
// OLD — treats everything as a normal exit
(code as u32) << 8
// (-11i32 as u32) << 8 = 0xFFFFF500
```

Linux userspace decodes this with the standard macros:

- `WIFEXITED(0xFFFFF500)` = `(status & 0x7F) == 0` → **true** (wrong!)
- `WEXITSTATUS(0xFFFFF500)` = `(status >> 8) & 0xFF` = **245**

So the shell sees exit code 245 instead of "killed by signal 11". Because this
looks like a normal exit, ash doesn't report any error and just stores 245 as
`$?`.

## Fix

### 1. Copy mmap regions during fork (`src/process.rs`)

`fork_process` now iterates the parent's `mmap_regions` and copies each one
page-by-page into the child's address space, up to an 8 MB cap
(`MAX_FORK_MMAP_PAGES = 2048` pages) to avoid OOM from giant file mappings.
Regions that exceed the cap are skipped with a log message.

### 2. Correct wait status encoding (`src/syscall.rs`)

A new helper `encode_wait_status` produces Linux-compatible wait status values:

```rust
fn encode_wait_status(code: i32) -> u32 {
    if code < 0 {
        // Signal death: low 7 bits = signal number
        ((-code) as u32) & 0x7F
    } else {
        // Normal exit: (code & 0xFF) << 8
        ((code as u32) & 0xFF) << 8
    }
}
```

Applied in `sys_wait4` (both specific-PID and any-child paths) and
`sys_waitpid`.

With this encoding:
- Normal exit 0 → status `0x0000` → `WIFEXITED` true, `WEXITSTATUS` 0
- SIGSEGV (−11) → status `0x000B` → `WIFSIGNALED` true, `WTERMSIG` 11

## Affected Scenarios

- `busybox.static sh` pipes (`echo hello | grep e`) now work
- Any Linux binary that forks children for built-in commands (no execve)
- Any process killed by a signal now reports the correct signal to its parent
  via waitpid/wait4

## Bug 3: `set_brk` page mapping starts at non-aligned address

### Problem

When the ELF loader sets the initial `brk`, it uses the exact end of the last
segment (e.g., `0x4311d0`), which is typically not page-aligned. The `set_brk`
method used this raw value as the starting VA for its page-mapping loop:

```rust
let old_top = self.brk;       // e.g. 0x4311d0
while page < aligned {
    alloc_and_map(page, ...);  // page = 0x4311d0 — NOT page-aligned!
    page += 0x1000;            // 0x4321d0, 0x4331d0, ...
}
```

`map_page` rejects non-page-aligned VAs (`"Addresses must be page-aligned"`),
so every `alloc_and_map` call in the first `set_brk` invocation silently failed
(the error was discarded by `let _ = ...`). No new pages were actually mapped.

This didn't immediately crash because the ELF loader pre-allocates 16 pages
(64 KB) beyond the last segment, providing a buffer for early heap use. But it
meant:

1. The first brk extension mapped zero pages (all calls failed silently)
2. After the first call, `self.brk` became page-aligned, so subsequent calls
   worked — but they re-mapped the pre-allocated pages (allocating new zeroed
   frames and overwriting the L3 entries, leaking the original frames)

### Fix

Page-align `old_top` upward before starting the mapping loop:

```rust
let old_top = (self.brk + 0xFFF) & !0xFFF;
```

The partial page containing `self.brk` is already mapped by the ELF loader, so
mapping should start at the next page boundary. When `self.brk` is already
page-aligned (all calls after the first), this is a no-op.

## Diagnostic: Fork page copy and execve logging

Added diagnostic logging to help trace intermittent issues with pipe commands
in dash (e.g., `echo hello | grep e` sometimes produces "echo: not found"):

1. **`copy_range_phys` in `fork_process`**: Now counts copied vs. skipped pages
   and logs a warning when any pages are unmapped (skipped) during the fork
   copy. This detects address space holes that could cause child crashes.

2. **`sys_execve`**: Now logs the resolved path, arguments, and PID for every
   `execve` call (when `SYSCALL_DEBUG_INFO_ENABLED` is true). If a child
   process calls `execve("echo")` instead of running it as a builtin, this
   log will confirm that dash's `find_command` failed to identify it as a
   builtin — pointing to a memory corruption or copy issue in the forked
   child's address space.
