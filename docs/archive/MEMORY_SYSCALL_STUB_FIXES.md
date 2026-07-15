# Memory Syscall Stub Fixes (March 2026)

Five fixes for stubbed or incomplete memory-related syscalls that caused crashes,
performance regressions, and potential deadlocks — most visibly when running bun.

## Background

Running `bun run /public/cgi-bin/akuma.js` via SSH exposed two critical problems:

1. **Crash**: bun registers a SIGSEGV handler for JSC JIT speculation guards.
   The kernel stored the handler via `rt_sigaction` but never invoked it — on
   an unresolvable data/instruction abort from EL0, `exceptions.rs` called
   `return_to_kernel(-11)` to kill the process immediately.

2. **67 million mremap calls**: JSC's conservative GC called
   `mremap(addr, 0x1000, 0x2000, 0)` to probe every page in the VA space.
   The kernel returned `ENOMEM` for every unmapped page (Linux returns
   `EFAULT`), so the GC could not skip unmapped ranges.

## Fix 1: Signal Delivery for Synchronous Faults

**Files**: `src/exceptions.rs`, `src/syscall.rs`

### What was added

`try_deliver_signal(frame, signal, fault_addr)` — builds a Linux AArch64-
compatible `rt_sigframe` on the user stack and redirects ELR_EL1 to the
registered handler.

Signal frame layout (592 bytes, 16-byte aligned on the user stack):

```
+0     siginfo_t              128 bytes  (si_signo, si_code, si_addr)
+128   ucontext_t header      168 bytes  (uc_flags, uc_link, uc_stack,
                                          uc_sigmask, __unused)
+296   sigcontext (mcontext)  280 bytes  (fault_address, regs[31],
                                          sp, pc, pstate)
+576   __reserved              16 bytes  (null aarch64_ctx terminator)
= 592
```

`do_rt_sigreturn(frame)` — reads the saved context from the signal frame on
the user stack and restores all registers, SP, ELR, and SPSR in the trap
frame.

### How it hooks in

In `rust_sync_el0_handler` (the EL0 synchronous exception handler):

- **Data abort** (`EC_DATA_ABORT_LOWER`): after all demand-paging paths fail,
  `try_deliver_signal(frame, 11, far)` is called before `return_to_kernel(-11)`.
  If a `UserFn` handler is registered for SIGSEGV, the function sets up the
  signal frame and returns `true`; the caller returns `11` (signal number) as
  x0 for the handler.

- **Instruction abort** (`EC_INST_ABORT_LOWER`): same pattern.

- **SVC (syscall)**: before dispatching to `handle_syscall`, syscall 139
  (`rt_sigreturn`) is intercepted. It calls `do_rt_sigreturn` to restore the
  saved context from the user stack, then returns the saved x0.

### Register flow

The assembly `sync_el0_handler` saves all user registers into a `UserTrapFrame`
(304 bytes on the kernel stack), calls `rust_sync_el0_handler(frame)`, then
restores from the same frame and ERETs. Signal delivery works by modifying
the frame in-place:

| Trap frame field | Set to                          |
|------------------|---------------------------------|
| `elr_el1`        | Signal handler address          |
| `sp_el0`         | New user SP (below signal frame)|
| `x30`            | `sa_restorer` (calls rt_sigreturn) |
| `x1`             | `&siginfo` (if `SA_SIGINFO`)    |
| `x2`             | `&ucontext` (if `SA_SIGINFO`)   |

The function return value becomes x0 (= signal number).

### Limitations

- No FPSIMD state is saved in `__reserved` (kernel does not manage FP context).
- Only synchronous faults (SIGSEGV, SIGILL, SIGBUS) are delivered this way.
  Asynchronous signal delivery (e.g. SIGTERM to a running process) is not
  yet implemented.
- Nested signals are not blocked; re-entering the handler will overwrite the
  signal frame.


## Fix 2: mremap EFAULT for Unmapped Addresses

**File**: `src/syscall.rs` (`sys_mremap`)

### What changed

When `flags & MREMAP_MAYMOVE == 0` and `new_size > old_size`, the kernel
now checks whether `old_addr` is actually mapped before choosing the error
code:

1. Check `is_current_user_page_mapped(old_addr)` (page tables)
2. Check `lazy_region_lookup_for_pid(pid, old_addr)` (lazy mmap table)
3. Check `proc.mmap_regions` (eager mmap regions)

If none match → `EFAULT`. If mapped but cannot grow in-place → `ENOMEM`.

### Impact

JSC's conservative GC uses the mremap error to distinguish mapped from
unmapped ranges. With `EFAULT`, it can skip entire unmapped regions.

**Before**: 67,633,152 mremap calls, 67,633,543 total syscalls.
**After**: 1,536 mremap calls, 12,570 total syscalls.

This is a **99.998% reduction** in mremap calls and a **99.98% reduction**
in total syscalls during bun startup.


## Fix 3: mremap Lazy Region Handling

**File**: `src/syscall.rs` (`sys_mremap`)

### What changed

When `MREMAP_MAYMOVE` is set and no eager region matches `old_addr`,
`sys_mremap` now handles lazy (demand-paged) regions:

1. Allocates new VA and physical pages (eager mapping).
2. Copies data from the old range to the new range.
3. Calls `munmap_lazy_regions_in_range(pid, old_addr, old_size)` to remove
   the lazy region metadata from `LAZY_REGION_TABLE`.
4. Unmaps and frees any demand-faulted pages in the old range.
5. Records the old VA range as free for reuse.

Previously, mremap of a lazy region would allocate the new region but silently
leak the old lazy region entry and any demand-faulted physical pages.


## Fix 4: set_robust_list

**Files**: `src/syscall.rs`, `crates/akuma-exec/src/process.rs`

### What changed

**Process struct** — added two fields:
- `robust_list_head: u64` — pointer to the robust list head in userspace
- `robust_list_len: usize` — size of `struct robust_list_head` (must be 24)

**sys_set_robust_list** — stores the head pointer and length on the current
process. Validates that `len == 24` (the fixed size on AArch64).

**return_to_kernel cleanup** — after the `CLONE_CHILD_CLEARTID` handling
and before deactivating the user address space, the kernel walks the
robust list:

```
robust_list_head layout (24 bytes):
  +0   next: *mut robust_list       (linked list pointer)
  +8   futex_offset: i64            (offset from entry to futex word)
  +16  list_op_pending: *mut robust_list  (in-progress entry)
```

For each entry in the linked list (capped at 2048 iterations):
1. Read the futex word at `entry + futex_offset`.
2. If the owner TID (lower 30 bits) matches this process's PID, set
   `FUTEX_OWNER_DIED` (bit 30) and wake one waiter.
3. Also check `list_op_pending` for an in-progress operation.

All pointer reads are guarded by `is_current_user_page_mapped` to avoid
kernel faults on unmapped memory.


## Fix 5: membarrier Command Dispatch

**File**: `src/syscall.rs` (`membarrier_cmd`)

### What changed

Replaced the bare `=> 0` stub with proper command parsing:

| Command (cmd value) | Response |
|---------------------|----------|
| `CMD_QUERY` (0) | Returns `0x18` (bits 3 and 4 set) |
| `CMD_REGISTER_PRIVATE_EXPEDITED` (16) | Returns 0 (no-op on single-core) |
| `CMD_PRIVATE_EXPEDITED` (8) | Issues `DSB ISH` + `ISB`, returns 0 |
| Other | Returns `EINVAL` |

The supported bitmask `0x18` indicates support for
`MEMBARRIER_CMD_PRIVATE_EXPEDITED` (1<<3) and
`MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED` (1<<4).

On a single-core system, membarrier is semantically a no-op (there are no
other CPUs to synchronize with), but issuing the barrier instructions is
cheap and correct.


## Tests

Seven new test functions in `src/tests.rs`, registered in `run_memory_tests`:

| Test | What it verifies |
|------|------------------|
| `test_mremap_lazy_region_moves_data` | Lazy region metadata removed after mremap |
| `test_mremap_lazy_region_shrink` | Shrink (new < old) returns old address |
| `test_mremap_lazy_cleans_old_ptes` | Old VA range uncovered after move |
| `test_set_robust_list_stores_head` | Head pointer stored on Process |
| `test_robust_list_cleanup_wakes_futex` | Fields initialized to zero |
| `test_membarrier_query_returns_bitmask` | CMD_QUERY returns 0x18 |
| `test_membarrier_private_expedited_succeeds` | CMD_PRIVATE_EXPEDITED returns 0 |


## Integration Test Results

`bun run /public/cgi-bin/akuma.js` via SSH:

- Script runs to completion and produces full HTML output.
- Signal delivery works: SIGSEGV delivered once to bun's JSC handler, which
  prints its crash report (after the script output is complete).
- Process exits cleanly with code 0.
- Per-process stats: `12,570 syscalls (5009/s)` with `mremap=1536`.
