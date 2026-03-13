# Fix: EL1 Crash During epoll_pwait / Stack Overflow in Bun

## Date

2026-03-13

## Problem

Running `opencode` (Bun-based) triggered two crashes in close succession:

### Crash 1 — EL1 data abort (kernel panic)

```
EC=0x25  — Synchronous Data Abort from current EL (kernel, EL1)
FAR=0x50004000
DFSC=0x07 — Translation fault, level 3 (leaf PTE not present)
STR x11, [x9]  (x9 = 0x50004000)
```

The kernel panicked instead of recovering. The faulting write was to a user VA
inside Bun's JSC mmap (`[0x50000000, 0x90000000)`).

### Crash 2 — User process SIGSEGV (stack overflow)

```
[WILD-DA] pid=44 FAR=0x203ffdff60 ELR=0x2d1d870 last_sc=113
[signal] sig 11 frame page 0x203ffdf000 not mappable
Process 44 (/bin/opencode) SIGSEGV after 0.63s
```

Bun uses more than 512 KB of stack during startup (JSC initialization is ~600 KB).
The eagerly-allocated stack had no lazy backing region below it, so growth past the
initial allocation caused an unhandled translation fault. Signal delivery then also
failed because the signal frame page was equally unmapped.

---

## Root Causes

### Bug 1 — EL1 faults killed the kernel, not just the process

`rust_sync_el1_handler` in `src/exceptions.rs` always called `wfe` (infinite spin)
on any synchronous EL1 exception.  A bad user pointer reaching a kernel
`write_unaligned` or `copy_nonoverlapping` would take an EC=0x25 abort and hang
the kernel permanently.

### Bug 2 — Epoll instances leaked on close / process exit

`EPOLL_TABLE` (a global `BTreeMap<u32, EpollInstance>`) was never cleaned up.
`sys_close` had a catch-all `_ => {}` arm that skipped `EpollFd` entirely. Every
process that used epoll leaked its `EpollInstance` and all `EpollEntry` nodes until
the next reboot.

### Bug 3 — User stack had no demand-paged growth region

`load_elf_with_stack` eagerly allocates exactly `compute_user_stack_size(RAM)` bytes
for the stack (512 KB on 1 GB RAM).  No lazy region was registered to back stack
growth beyond that limit. The fault handler's `lazy_region_lookup` found nothing for
addresses below the eager stack bottom and delivered SIGSEGV.

### Non-issue — `validate_user_ptr` kernel VA exclusion (reverted)

An earlier attempt to guard against EC=0x25 by excluding `[0x4000_0000, 0x6000_0000)`
from valid user addresses was reverted. Bun's JSC heap starts at `0x50000000`, so
the exclusion incorrectly returned EFAULT for legitimate path strings allocated in
that heap (e.g. `"//package.json"`). Since the kernel VA and user mmap ranges
overlap on this system, a simple VA-range check cannot distinguish them reliably.

---

## Fixes

### Fix 1 — EL1 data-abort recovery (`src/exceptions.rs`)

When EC=0x25 fires with ELR in kernel code range `[0x4020_0000, 0x6000_0000)`,
instead of looping with `wfe`, the handler now:

1. Marks the current process as Zombie (exit code −14 / EFAULT)
2. Calls `kill_thread_group` to terminate all threads of the process
3. Returns — the scheduler will not resume the dead process

This means a bad user pointer that leaks past `validate_user_ptr` degrades to a
process kill, not a kernel panic.

### Fix 2 — Epoll cleanup on close (`src/syscall/poll.rs`, `src/syscall/fs.rs`)

Added `pub(super) fn epoll_destroy(epoll_id: u32)` that removes the instance from
`EPOLL_TABLE`. `sys_close` now matches `EpollFd` and calls `epoll_destroy`.

### Fix 3 — Lazy demand-paged stack region (`crates/akuma-exec/src/process/mod.rs`)

After the eager stack allocation, all four process-creation paths (`from_elf`,
`from_elf_path`, `replace_image`, `replace_image_from_path`) now register a 32 MB
lazy anonymous region covering `[stack_top − 32 MB, stack_top)`.

```
const LAZY_STACK_MAX: usize = 32 * 1024 * 1024;
let lazy_stack_start = stack_top.saturating_sub(LAZY_STACK_MAX);
push_lazy_region(pid, lazy_stack_start, LAZY_STACK_MAX, RW_NO_EXEC);
```

Pages within the eager stack are already mapped; the lazy region covers growth
beyond them at no physical-memory cost until actually faulted. This also fixes
signal delivery to a process that has grown its stack (the signal frame page is now
within the lazy region and can be demand-paged).

---

## Files Changed

| File | Change |
|------|--------|
| `src/exceptions.rs` | EL1 abort recovery — kill process on EC=0x25 in kernel code |
| `src/syscall/poll.rs` | Added `epoll_destroy()` |
| `src/syscall/fs.rs` | `sys_close` — added `EpollFd` arm calling `epoll_destroy` |
| `crates/akuma-exec/src/process/mod.rs` | 32 MB lazy stack region in all exec paths |

---

## Why the validate_user_ptr Kernel-VA Check Was Removed

The original EC=0x25 crash plan proposed excluding `[KERNEL_VA_BASE, KERNEL_VA_END)`
from user pointers. This was added and then reverted because:

- Kernel code is loaded at physical/virtual `0x40200000` and the kernel heap extends
  upward from there. With 1 GB RAM the heap ceiling is around `0x50200000`.
- Bun/JSC requests a 1 GB anonymous mmap starting at `0x50000000` (the "gigacage").
- The ranges `[0x50000000, 0x50200000)` overlap — any fixed VA exclusion that covers
  the kernel heap also rejects legitimate user addresses.
- The EL1 recovery (Fix 1) is the correct architectural answer: if a write to a user
  VA faults at EL1, the kernel recovers without a panic.
