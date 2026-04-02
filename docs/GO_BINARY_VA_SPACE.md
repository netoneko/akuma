# Go Binary VA Space Fix (forktest_parent OOM)

## Date

2026-03-29

---

## Problem

The `forktest_parent` Go binary (statically linked, no `PT_INTERP`) crashed at startup:

```
runtime: out of memory: cannot allocate 4194304-byte block (0 in use)
```

The Go runtime panicked during heap initialization (`internal/cpu.doinit`), before
any user code ran.  The kernel log showed:

```
[mmap] REJECT: pid=49 size=0x4000000 next=0x3cb70000 limit=0x3f700000
[mmap] REJECT: pid=49 size=0x8000000 next=0x3cb70000 limit=0x3f700000
```

Only ~43 MB of VA space remained when Go tried to allocate 64 MB heap arenas.

---

## Root Cause

### 1. compute_stack_top threshold too high

`crates/akuma-exec/src/elf/mod.rs` — `compute_stack_top(brk, has_interp)` assigned
the 1 GB default VA space to any static binary with loaded segments ending below 4 MB:

```rust
if !has_interp && brk < 0x400_0000 {   // 4 MB — too high
    return DEFAULT;  // 1 GB
}
```

`forktest_parent` is a Go statically-linked binary.  The Go runtime is embedded in the
binary but the total loaded segments ended at ~2 MB (`brk < 4 MB`), triggering the 1 GB
path.  The mmap limit was therefore only `~0x3F700000` (~1 GB).

### 2. Go arenaHint probing permanently consumes VA

During heap initialisation the Go runtime probes candidate arena base addresses
(`arenaHints`) by calling:

```
mmap(hint=4GB+k*64MB, size=64MB, PROT_NONE, MAP_ANON|MAP_PRIVATE, -1, 0)
```

On Linux, when `hint` is free the kernel returns exactly `hint`.  On Akuma, hints
are ignored — the kernel returns the next available VA from `next_mmap` instead.
Because the returned address ≠ `hint`, Go calls `munmap` to discard it and tries
the next hint.

On Akuma, `PROT_NONE` allocations are lazy (no physical pages).  By design, lazy
`munmap` does **not** recycle the VA back into `free_regions` — doing so would cause
an infinite `mmap→reject→munmap→same-addr` loop (observed with Go's heap prober
returning the same address 60+ times in a row).

Each failed probe therefore **permanently consumes 64 MB** of the bump-allocator VA.

### 3. Exhaustion arithmetic

```
VA budget:  mmap_limit (≈ 1 GB) - next_mmap_initial ≈ 1 GB
Per probe:  64 MB (one heapArenaBytes)
Probes fit: 1 GB / 64 MB ≈ 15
Go tries:   up to 128 arenaHints
```

After ~15 probes `alloc_mmap` returns `None`, the kernel returns `MAP_FAILED`, and
Go panics with "out of memory: cannot allocate 4194304-byte block (0 in use)".

---

## Fix

The threshold was lowered from 4 MB to 512 KB in `compute_stack_top`:

```rust
// Before
if !has_interp && brk < 0x400_0000 {   // 4 MB
    return DEFAULT;
}

// After
const SMALL_STATIC_THRESHOLD: usize = 0x8_0000; // 512 KB
if !has_interp && brk < SMALL_STATIC_THRESHOLD {
    return DEFAULT;
}
```

Binaries that exceed 512 KB now receive the large VA layout (128 GB mmap space,
256 GB stack top), matching dynamically-linked binaries.

### Threshold rationale

| Binary type | Typical brk | VA space assigned |
|-------------|-------------|------------------|
| musl-libc static C program | < 200 KB | 1 GB (DEFAULT) |
| TCC-compiled C program | < 200 KB | 1 GB (DEFAULT) |
| Minimal Go program (embedded runtime) | > 1 MB | Large (128 GB mmap) |
| `forktest_parent` | ~2 MB | Large (fixed) |
| Any `PT_INTERP` binary | any | Large (unchanged) |

512 KB sits safely in the gap between the two populations.  No known static C
binary (musl, uclibc, TCC) approaches 512 KB.  No Go binary can be built below
~1 MB because the Go runtime itself is ~1 MB of text + data.

---

## Files Changed

| File | Change |
|------|--------|
| `crates/akuma-exec/src/elf/mod.rs` | `compute_stack_top`: threshold `0x400_0000` → `0x8_0000`; new constant `SMALL_STATIC_THRESHOLD`; updated doc comment |
| `src/tests.rs` | Four new regression tests; registered in memory test runner |
| `docs/GO_BINARY_VA_SPACE.md` | This file |

---

## Tests Added

| Test | What it verifies |
|------|-----------------|
| `test_compute_stack_top_small_static` | `brk < 512 KB` → returns DEFAULT (1 GB) |
| `test_compute_stack_top_go_sized_static` | `brk ≥ 512 KB`, no interp → `stack_top > DEFAULT` (large VA) |
| `test_compute_stack_top_boundary_512k` | Exact boundary: 511 KB → DEFAULT; 512 KB → large VA |
| `test_go_binary_va_exhaustion_scenario` | 1 GB budget fits < 128 × 64 MB probes; large VA budget fits all 128 |

---

## Related

- `docs/EPOLL_EL1_CRASH_FIX.md` — related process crash fixes
- `src/syscall/mem.rs` — `sys_mmap` REJECT logging and lazy-PROT_NONE non-recycling
- `crates/akuma-exec/src/process/types.rs` — `ProcessMemory::alloc_mmap` bump allocator

---

## Follow-up Fix: fork code_start SIGSEGV (2026-03-31)

After the VA-space fix above, `forktest_parent` could start but each `clone(CLONE_VFORK)` child
SIGSEGV'd at the same address:

```
[DP] no lazy region for inst FAR=0xa4600 pid=57
[Fault] Instruction abort from EL0 at FAR=0xa4600, ISS=0x7
[Fault] Process 57 (/bin/forktest_parent) SIGSEGV after 0.02s
```

### Root cause

`fork_process` (`crates/akuma-exec/src/process/mod.rs`) used a hardcoded `code_start = 0x400000`
(4 MB) when deciding which VA range to share with the child:

```rust
let code_start = if parent.memory.code_end >= 0x1000_0000 {
    0x1000_0000
} else {
    0x400000   // ← too high for Go ARM64 binaries
};
if parent.brk > code_start {   // ← FALSE for brk=0x229000
    // copy code range ... (SKIPPED!)
}
```

Go ARM64 binaries load at ~`0x40000` (64 KB).  `forktest_parent` had `brk=0x229000` (< 4 MB),
so the condition was false and **no code pages were copied to the child**.  The child inherited
only the 13 heap/mmap lazy regions, with nothing covering the text segment at `0xa4600`.

### Fix

`code_start` for the small-binary branch was changed from `0x400000` to `mmu::PAGE_SIZE`
(0x1000).  `cow_share_range` / `copy_range_phys` iterate only actually-mapped PTEs, so
scanning from 0x1000 instead of 0x400000 costs nothing for pages that aren't mapped.

This is conditioned on `code_end < 0x400000` so standard musl/TCC binaries (which load at
exactly 0x400000) keep using `0x400000` as their floor and are not affected.  Applied to both
the CoW fork path and the eager-copy fork path.

### Files changed

| File | Change |
|------|--------|
| `crates/akuma-exec/src/process/mod.rs` | CoW path line ~1334 and eager-copy path line ~1443: `0x400000` → `mmu::PAGE_SIZE` |
| `src/process_tests.rs` | Four new regression tests (see below) |

### Tests added

| Test | What it verifies |
|------|-----------------|
| `test_fork_code_start_low_va_is_covered` | crash VA 0xa4600 is within `[PAGE_SIZE, 0x229000)` with the fixed code_start |
| `test_fork_code_start_not_skipped_when_brk_lt_400k` | `brk(0x229000) > PAGE_SIZE` → condition is true, copy proceeds |
| `test_fork_code_start_large_binary_unchanged` | `code_end >= 0x1000_0000` still picks 0x1000_0000; `code_end` at 0x400000 keeps 0x400000 |
| `test_fork_brk_len_no_underflow_go_binary` | old bug skipped, new path produces correct non-zero `brk_len` |

---

## Follow-up Fix: vfork signal-interrupted wait (2026-04-01)

After the code_start fix, children could start executing but then deadlocked — never calling
`execve`.  The parent appeared to unblock immediately from vfork, before the child ran at all:

```
[T13.02] [clone] flags=0x4111 stack=0x0
...
[signal] tkill(tid=12, sig=23)          ← Go SIGURG goroutine-preemption signal
[FORK-COW] shared 5200 pages in 14082µs
[T13.03] [FORK-DBG] VFORK blocking parent tid=12 child=53
[T13.03] [FORK-DBG] VFORK parent tid=12 RESUMED after child=53   ← instant resume!
[FORK-DBG] trampoline ENTRY tid=17
[TMR] t=1500 T=17 f=0                  ← child spinning with no progress
```

### Root cause

Go's goroutine scheduler sends `SIGURG` (signal 23) to preempt goroutines.  The signal
arrives while the CoW fork is in progress.  `pend_signal_for_thread()` calls `wake()`,
which sets the `WOKEN_STATES[tid]` sticky flag.  When the parent then calls
`schedule_blocking(u64::MAX)` for the vfork wait, it finds that flag already set and
returns **immediately** — before the child has executed a single instruction.

Both parent and child now run concurrently.  The child's CoW copy of the parent's address
space includes any Go runtime spinlocks that were held at fork time.  The child cannot
release those locks (the goroutine that holds them doesn't exist in the child), so it spins
forever: `[TMR] T=17 f=0` repeating every 500 ms.

### Fix

The single `schedule_blocking` call is replaced with a loop that re-blocks whenever
`VFORK_WAITERS` still contains the child PID — meaning `vfork_complete` has not fired:

```rust
loop {
    akuma_exec::threading::schedule_blocking(u64::MAX);
    let still_pending = crate::irq::with_irqs_disabled(|| {
        VFORK_WAITERS.lock().contains_key(&new_pid)
    });
    if !still_pending { break; }
    // Signal caused the wake — re-block until child calls execve/exit
}
```

Signal wakes are absorbed silently.  The signal remains in `PENDING_SIGNAL` and is
delivered normally when the clone syscall returns to EL0 after vfork_complete fires.

### Files changed

| File | Change |
|------|--------|
| `src/syscall/proc.rs` | vfork block: single `schedule_blocking` → loop until `VFORK_WAITERS` entry gone; two test helpers added |
| `src/process_tests.rs` | `test_vfork_signal_wake_is_reblocked` regression test |

### Test added

| Test | What it verifies |
|------|-----------------|
| `test_vfork_signal_wake_is_reblocked` | After a simulated "signal wake" the VFORK_WAITERS entry is still present; a subsequent `vfork_complete` correctly removes it |

---

## Follow-up Fix: EL1 crash — missing THREAD_PID_MAP + CoW write (2026-04-01)

After the vfork-signal fix, `forktest_parent` could start children but then crashed with an EL1
SYNC EXCEPTION (EC=0x25 — data abort from kernel) the moment the child called `clone` to create
a Go OS thread:

```
[FORK-DBG] EL1 SYNC EXCEPTION!
  ELR=0x403c0260, FAR=0x1e0086ba8
  Thread=17, Instruction: 0xb9000377 (STR W23, [X27])
  Killing PID 53 (/bin/forktest_parent)
[FORK-DBG] vfork_complete child_pid=53
```

Two bugs were found, each independently fatal.

### Bug 1 — fork_process missing THREAD_PID_MAP entry

`fork_process` allocated a new thread and stored `new_proc.thread_id = Some(tid)` but never
inserted `(tid → child_pid)` into `THREAD_PID_MAP`.

`current_process()` first checks `THREAD_PID_MAP`; if absent it falls back to reading
`PROCESS_INFO_ADDR` — which still holds the **parent's** PID (the child has not yet set up its
own info page).  So any kernel code running on the child thread (e.g. the exception handler or
the EL1 abort path) saw PID 53 (parent) instead of PID 57 (child).

Consequence: the EL1 abort handler killed PID 53 and called `vfork_complete(53)`.  But the
parent (PID 53, tid 11) was waiting for `vfork_complete(57)` to remove its VFORK_WAITERS
entry — it never arrived, so the parent stayed blocked forever.

**Fix:** added `THREAD_PID_MAP.lock().insert(tid, child_pid)` in `fork_process` immediately
after `new_proc.thread_id = Some(tid)`, mirroring what `clone_thread` already did.

### Bug 2 — clone_thread plain EL1 store to CoW-RO page

`clone_thread` wrote `child_pid` to `parent_tid_ptr` / `child_tid_ptr` via a plain
`core::ptr::write(ptr as *mut u32, child_pid)`.  When the caller is a vfork child, its pages
are CoW-marked read-only.  The kernel `str` instruction (EL1) faults immediately: EC=0x25,
ISS=0x4f → DFSC=0x0f (permission fault level 3).

**Fix:** replaced both plain writes with `copy_to_user_safe(...)`, which installs a per-thread
EL1 fault recovery handler.  If the page is RO (CoW or truly unmapped), the fault handler
returns EFAULT instead of panicking the kernel.  The `_` discard on the Result is deliberate —
Linux also does not return an error from `clone` if SETTID writes fail.

### Files changed

| File | Change |
|------|--------|
| `crates/akuma-exec/src/process/mod.rs` | `fork_process`: add `THREAD_PID_MAP.lock().insert(tid, child_pid)` after thread alloc |
| `crates/akuma-exec/src/process/mod.rs` | `clone_thread`: `copy_to_user_safe` attempted then reverted (see below) |
| `src/process_tests.rs` | Two new regression tests (see below) |

### Tests added

| Test | What it verifies |
|------|-----------------|
| `test_fork_thread_pid_map_invariant` | Logical invariant: child tid must resolve to child_pid, not parent_pid |
| `test_clone_thread_tid_write_cow_safe` | Bits-32+ guard catches all negative error codes before clone_thread |

### Reverted: copy_to_user_safe for clone_thread TID writes

`copy_to_user_safe` was used to replace `core::ptr::write` for the `parent_tid_ptr`
and `child_tid_ptr` writes in `clone_thread`.  The motivation was preventing EL1
crashes when a vfork child called `clone_thread` with CoW-RO pages.

**Problem:** `copy_to_user_safe`'s byte-by-byte `strb` through the fault-handler
mechanism silently returned EFAULT on some pages, leaving Go's `mp.procid = 0`.  The
Go runtime then dereferenced a nil m-pointer, crashing at FAR=0x88 (nil + struct
field offset) during goroutine thread startup.

**Fix:** Reverted to plain `core::ptr::write`.  The CoW-RO scenario that motivated
`copy_to_user_safe` can no longer occur: the bits-32+ guard in `sys_clone_pidfd`
rejects all garbage flags (negative error codes) before they reach `clone_thread`.
Only legitimate `CLONE_THREAD|CLONE_VM` requests with writable pages enter
`clone_thread`.

---

## Follow-up Fix: clone flag routing and EL1 fault handler (2026-04-02)

### Problem 1: clone(flags=0) fork bomb

Go's vfork child calls `clone(flags=0)` due to register-state leakage in the
`rawVforkSyscall` → `forkAndExecInChild1` path.  The initial fix routed all
non-`CLONE_THREAD|CLONE_VM` flags to `fork_process`, which turned `clone(0)` into a
successful fork.  Each fork child ran the Go scheduler, calling `newosproc` → `clone` →
another fork, creating an infinite fork loop.

The old behaviour (ENOSYS) was actually correct for this case: Go's error handling
absorbed the -38 return and the vfork child continued to `execve`.  The SIGSEGV from
the accidental `clone_thread(stack=0)` was on a **different** thread and didn't affect
the calling thread's execution.

**Fix (routing):** Restored the original fork-routing condition.  Only `CLONE_VFORK` or
`SIGCHLD` (0x11 in the low byte) routes to `fork_process`.  `clone(flags=0)` returns
ENOSYS as before.

**Fix (garbage flags):** Added an early check: `if flags >> 32 != 0 { return ENOSYS; }`.
No valid clone flag uses bits 32+.  Garbage values like -ENOSYS (0xffffffffffffffda) and
-EAGAIN (0xfffffffffffffff5) have those bits set.  Without this guard, the error code
became the next clone's flags, which still matched `CLONE_THREAD|CLONE_VM`, creating an
infinite loop: clone(-38)→EAGAIN(-11)→clone(-11)→EAGAIN(-11)→...

### Problem 2: EL1 user-copy fault handler noise (NOT fixed — deadlock risk)

The EL1 sync exception handler prints a debug dump before checking the user-copy
fault handler.  `copy_to_user_safe` faults produce noisy output even when handled.

An initial fix moved `get_user_copy_fault_handler()` before the debug dump, but this
caused a **deadlock**: `get_user_copy_fault_handler()` acquires POOL lock inside
`with_irqs_disabled`.  If an EL1 data abort fires while POOL lock is already held
(e.g. during context switch or thread spawn), the nested lock acquisition hangs the
kernel.  The fix was reverted; the debug dump noise is acceptable.

### Files changed

| File | Change |
|------|--------|
| `src/syscall/proc.rs` | `sys_clone_pidfd`: restored CLONE_VFORK\|SIGCHLD fork condition; unknown flags → ENOSYS |
| `src/exceptions.rs` | `rust_sync_el1_handler`: fast-path attempted then reverted (deadlock risk documented) |
| `src/process_tests.rs` | New regression test (see below) |

### Tests added

| Test | What it verifies |
|------|-----------------|
| `test_clone_flags_routing` | Verifies routing: VFORK/SIGCHLD→fork, THREAD\|VM→thread, flags=0→enosys, garbage→enosys |

---

## Follow-up Fix: clone_thread stack=0 crash guard (2026-04-02)

### Problem

After the clone flag routing fix, the ENOSYS cascade still produces `clone(flags=-38)`
calls that enter `clone_thread` (because 0xffffffffffffffda has `CLONE_THREAD|CLONE_VM`
bits set).  `clone_thread` accepted `stack=0`, creating a thread with SP=0 that
immediately crashed at FAR=0x28 (null-pointer + struct field offset) with SIGSEGV.

The crash pattern repeats every ~0.3s:

```
[clone] flags=0x0 stack=0x0           → ENOSYS
[clone] flags=0xffffffffffffffda      → clone_thread(stack=0)
[FORK-DBG] trampoline ENTRY tid=18
[DP] no lazy region for FAR=0x28      → SIGSEGV
[Fault] Process 54 (/bin/forktest_parent) SIGSEGV after 0.24s
```

Each bogus thread consumes a thread slot and triggers `vfork_complete(wrong_pid)` —
never the vfork child's PID — so the parent stays permanently blocked.

### Root cause

Go's vfork child leaks register state: after `rawVforkSyscall(SYS_CLONE3, ...)`,
the child's R0=0 and R8=435 (SYS_CLONE3).  Go's child path code eventually makes
syscalls where leftover register values produce `clone(flags=0, stack=0)`.  The
ENOSYS return (-38) in R0 becomes the flags for the next clone call.  `-38` =
`0xffffffffffffffda` has bits 8 (`CLONE_VM`) and 16 (`CLONE_THREAD`) set, so the
kernel enters `clone_thread` with `stack=0`.

### Fix

`clone_thread` now rejects `stack=0` with an error:

```rust
if stack == 0 {
    return Err("clone_thread: stack must be non-zero");
}
```

The caller (`sys_clone_pidfd`) returns EAGAIN (-11), which Go handles as a
non-fatal thread-creation failure.  No bogus thread is created, no SIGSEGV,
no wasted thread slots.

### Files changed

| File | Change |
|------|--------|
| `crates/akuma-exec/src/process/mod.rs` | `clone_thread`: reject stack=0 at entry |
| `src/process_tests.rs` | Two new regression tests (see below) |

### Tests added

| Test | What it verifies |
|------|-----------------|
| `test_clone_thread_rejects_zero_stack` | -ENOSYS has CLONE_THREAD\|CLONE_VM bits; stack=0 would crash; guard catches it |
| `test_clone_garbage_flags_cascade` | Full cascade: -38 and -11 caught by bits-32+ guard; PID-as-flags has no THREAD bits |

---

## Current state: vfork child clone loop (2026-04-02, open issue)

After all the fixes above, `forktest_parent` no longer crashes.  The kernel is stable:
no SIGSEGV, no EL1 exceptions, no thread slot exhaustion.  Shell commands, elftest, and
hello_musl work correctly through fork+execve.

However, `forktest_parent`'s vfork child (PID 53) never calls `execve`.  Instead it
enters an infinite `clone(-38) → ENOSYS → clone(-38) → ENOSYS` loop.  The parent stays
blocked on `VFORK_WAITERS` forever.

### What happens

1. Parent calls `clone3(CLONE_VFORK|CLONE_VM|CLONE_PIDFD|SIGCHLD)` → child PID 53
2. Child starts at the instruction after `SVC` in Go's `rawVforkSyscall`
3. Child writes `r1=0, err=0` to the stack → enters Go's child path
4. **Instead of calling `close/dup2/execve`, the child calls `clone(flags=0)`**
5. `clone(0)` → ENOSYS (-38)
6. Go uses -38 as the next clone flags → `clone(-38)` → ENOSYS (bits-32+ guard)
7. Loop forever

### Likely root cause

The vfork child's Go heap data (pointers to the exec path string, fd arrays, and
`SysProcAttr` struct) resides in CoW-shared or demand-paged memory.  Some of this data
may be zeroed in the child's address space if the backing pages were in lazy regions
that didn't get properly CoW-shared.  With zeroed pointers, `execve(nil)` would fail,
and Go's `childerror` path calls `exit(253)` — but the thread somehow doesn't terminate,
or the child takes the wrong branch before reaching `execve`.

The relative exec path `./forktest_child` also contributes: if the working directory is
`/`, the path resolves to `/forktest_child` which doesn't exist (the binary is at
`/bin/forktest_child`).

### Follow-up: copy_to_user_safe crash at FAR=0x88 (2026-04-03)

The `copy_to_user_safe` byte-by-byte `strb` in `clone_thread` for `parent_tid_ptr` /
`child_tid_ptr` writes silently returned EFAULT on some Go heap pages.  Go's
`mp.procid` was left as 0.  The Go runtime dereferenced a nil m-pointer during
goroutine thread startup → crash at FAR=0x88 (nil + struct field offset 0x88).

**Fix:** reverted to `core::ptr::write`.  The CoW-RO scenario that motivated
`copy_to_user_safe` can no longer occur thanks to the bits-32+ guard.

---

## Follow-up Fix: PROCESS_INFO_ADDR overwritten by cow_share_range (2026-04-03)

### Problem

The diagnostic `[clone] tid=17 pid=49` revealed the root cause of the vfork child's
clone loop: the child was reading `pid=49` (parent) from `PROCESS_INFO_ADDR` instead of
`pid=53` (child).  The child was running with the **parent's** process info mapping.

### Root cause

`PROCESS_INFO_ADDR = 0x1000 = PAGE_SIZE`.  For Go ARM64 binaries, `code_start = PAGE_SIZE`
because `code_end < 0x400000`.  The CoW fork's `cow_share_range(code_start, brk_len, ...)`
walks from `0x1000` to `0x229000`, copying parent PTEs to the child — including the PTE
at `0x1000` (PROCESS_INFO_ADDR).

fork_process allocated a new process info frame and mapped it at 0x1000 **before** the CoW
fork (step 2).  `cow_share_range` then **overwrote** this mapping with the parent's PTE
(step 4).  The child's process info frame was orphaned — correctly written with `pid=53`
via `phys_to_virt`, but unmapped from the child's address space.

The child read `pid=49` from PROCESS_INFO_ADDR → `current_process()` returned the parent →
`vfork_complete(wrong_pid)` → parent never unblocked.  The child also ran Go code in the
wrong process context, causing the clone(0) loop.

### Fix

Re-map `PROCESS_INFO_ADDR` to the child's own frame **after** all CoW sharing is done
(step 5, before writing the child PID).  This overwrites whatever PTE `cow_share_range`
installed.

```rust
// 5. Re-map PROCESS_INFO_ADDR after CoW (cow_share_range may have overwritten it)
let _ = new_proc.address_space.map_page(
    PROCESS_INFO_ADDR,
    new_proc.process_info_phys,
    mmu::user_flags::RO | mmu::flags::UXN | mmu::flags::PXN,
);
// Then write child PID to the frame
```

Standard musl/TCC binaries (code_start=0x400000) are unaffected because 0x400000 > 0x1000.

### Files changed

| File | Change |
|------|--------|
| `crates/akuma-exec/src/process/mod.rs` | `fork_process`: re-map PROCESS_INFO_ADDR after CoW sharing |
| `src/syscall/proc.rs` | `[clone]` log now includes tid and pid for diagnostics |
| `src/process_tests.rs` | Two new regression tests (see below) |

### Tests added

| Test | What it verifies |
|------|-----------------|
| `test_process_info_addr_cow_overwrite` | For Go binaries: code_start=PAGE_SIZE=PROCESS_INFO_ADDR → overlap confirmed |
| `test_process_info_addr_not_in_code_range_standard` | For musl/PIE binaries: code_start > PROCESS_INFO_ADDR → no overlap |

### Diagnostic: clone debug now includes tid and pid

`[clone]` log lines now include the calling thread ID and PID, making it possible to
distinguish whether `clone(0)` comes from the vfork child (PID 53) or from a parent
goroutine thread (PID 49).  `execve` logging also includes the resolved path and PID.

### What works

| Operation | Status |
|-----------|--------|
| Kernel boot + all tests | PASS |
| Shell fork+exec (elftest, hello_musl) | PASS |
| Go binary startup + goroutine threads | PASS (after copy_to_user_safe revert) |
| forktest_parent vfork child → execve | **Stuck in clone loop** (open) |
