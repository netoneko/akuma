# Go Forktest Crash Analysis

This document details crash patterns seen when running `forktest_parent` with **stress flags** (especially **`-combined_stress`** or **`-mmap_test`**) on Akuma OS. The **child** often shows `addr=0x2` in Go's allocator; the **parent** can fault in **`read()`** on the epoll pipe with a **heap-range** fault address (see [Isolation matrix](#isolation-matrix-2026-04-14)).

## Current status (2026-04-14)

**These crashes still reproduce** (for example: `panic: invalid memory address or nil pointer dereference`, `addr=0x2`, `pc≈0x86768` in `memclrNoHeapPointers` / `mallocgcLarge` inside `runMmapStress`).

This failure mode is **orthogonal** to ext2 fixes that removed spurious **`input/output error`** on `/tmp` under load (blocking `read_state()` and a single `write_state()` for `write_at` in [`crates/akuma-ext2/src/ext2.rs`](../crates/akuma-ext2/src/ext2.rs)). If you see **EIO** on temp files, that was filesystem contention; if you see **`addr=0x2`** in the Go allocator, treat it as the **open heap / CoW / demand-paging** investigation described below.

| What you see | Likely bucket | Where to read |
|--------------|----------------|---------------|
| `write /tmp/...: input/output error` | ext2 read path starved / `IoError` | ext2 history in `GO_FORK_EXEC_FIXES.md` |
| `addr=0x2`, panic in `mallocgc` / `memclr` | Go heap sees bad pointer (kernel memory model) | This file, §Crash Pattern 1–2 |

**Mitigations while debugging:** ample RAM (`MEMORY=2048M` or higher), `GODEBUG=asyncpreemptoff=1`, or avoid **`-mmap_test`** / **`-combined_stress`** until fixed. **`GOMAXPROCS=1` does not prevent** the **parent** `read()` SIGSEGV when **`-mmap_test`** is enabled ([Isolation matrix](#isolation-matrix-2026-04-14)).

## Isolation matrix (2026-04-14)

Shell: `export GOMAXPROCS=1` for all runs below. Command line: `forktest_parent --duration 10s` plus flags.

| Child mode | Outcome |
|------------|---------|
| **(none)** — children run default main (no stress) | **Stable.** Parent sends SIGTERM at deadline; `Wait()` reports `signal: killed`; empty child stdout. |
| **`-mmap_test`** | **Parent SIGSEGV** in `unix.Read` on pipe (**`main.go:199`**): `PC≈0x13060`, fault `addr≈0x1e39df000`, `syscall` read `fd=4`. Same shape as [Pattern 2](#crash-pattern-2-parent-process-heap-corruption). **Does not require** `-combined_stress` or multiple Go M-threads in the parent. |
| **`-file_io`** | **Stable** in this session: children print `Received terminated, exiting gracefully.` before kill. |
| **`-send_signal`** | **Stable** (benign race): `Failed to send SIGINT … process already finished` if the child exits before 500 ms; then deadline kill as usual. |

**Conclusion:** The **mmap heap stress in children** (`runMmapStress`, large `make([]byte, …)`) is enough to trigger failure; **`GOMAXPROCS=1` does not rule out “multi-M in parent”** as the sole cause—it rules out **parent** multi-threading as required for the parent `read()` crash. The bug is likely **kernel-side** (pipe/read path, scheduling, or memory accounting) or **child-driven** kernel state that affects the parent’s syscall return.

## Test Command

```bash
MEMORY=2048M cargo run --release
# Then via SSH:
cd /bin && forktest_parent --duration 10s --combined_stress
```

## Crash Pattern 1: Child Process Heap Corruption

### Symptoms

```
panic: runtime error: invalid memory address or nil pointer dereference
[signal SIGSEGV: segmentation violation code=0x1 addr=0x2 pc=0x86768]

goroutine 10 [running]:
main.runMmapStress(...)
runtime.memclrNoHeapPointers()
  .../memclr_arm64.s:91 +0xb8
runtime.mallocgcLarge(...)
  .../malloc.go:1612 +0x1a8
```

### Kernel Log Evidence

```
[DA-MISS] pid=96 ppid=90 va=0x2 lr_count=14 parent_lr=13 parent_has_va=false
[WILD-DA] pid=96 FAR=0x2 ELR=0x86768 last_sc=18446744073709551615
```

### Analysis

- **Fault address**: `0x2` is NOT a valid memory address - it's a corrupted pointer value
- **PC=0x86768**: Crash occurs in `memclrNoHeapPointers` (Go's memory zeroing routine)
- **Call chain**: `make([]byte, N)` → `mallocgc` → `mallocgcLarge` → `memclrNoHeapPointers`
- **`last_sc=!0u64`**: No syscall was active - crash is purely in userspace
- **Implication**: Go's `mallocgc` returned `0x2` instead of a valid heap pointer

## Crash Pattern 2: Parent Process Heap Corruption

### Symptoms

```
SIGSEGV: segmentation violation
PC=0x13060 m=0 sigcode=1 addr=0x2

goroutine 1 [syscall]:
syscall.Syscall(0x3f, 0x4, 0x1e0087718, 0x400)  // read() syscall
```

### Kernel Log Evidence

```
[DA-MISS] pid=90 ppid=0 va=0x2 lr_count=13 parent_lr=0 parent_has_va=false
[WILD-DA] pid=90 FAR=0x2 ELR=0x13060 last_sc=18446744073709551615
```

### Analysis

- **Fault address**: Older kernel captures reported **`FAR=0x2`** for the parent as well as the child. A **2026-04-14 SSH capture** (see below) shows the parent fault at **`addr=0x1e251f000`** (heap-range VA) while the child still shows **`addr=0x2`**. So the parent failure is **not always** the same bit pattern as the allocator bug in the child; it may be a **follow-on SIGSEGV** during `read()` (pipe drain), **kernel copy_to_user**, or a **distinct** runtime bug.
- **PC≈0x13060**: In Go's syscall path (e.g. return trampoline around `read`)
- **Context**: Parent was in **`unix.Read`** on the epoll-monitored pipe (**`fd=4`** in registers: `r0=4`, `r1=buf`, `r2=0x400`); corresponds to [`userspace/forktest/parent/main.go`](../../userspace/forktest/parent/main.go) pipe-read logic (line numbers shift with Go version; stack may show `main.go:176` in older builds vs current sources).
- **Timing**: Often **after** a child process has already panicked with **`addr=0x2`** in `runMmapStress`, but not always independently observed.

## Captured SSH log (2026-04-14)

Full command: `forktest_parent --duration 10s --combined_stress` from `/bin` over SSH.

**1. Child (`forktest_child`) — Pattern 1**

```
panic: runtime error: invalid memory address or nil pointer dereference
[signal SIGSEGV: segmentation violation code=0x1 addr=0x2 pc=0x86768]

goroutine 10 [running]:
main.runMmapStress(...{childID}...)
    .../forktest/child/main.go:88 +0x228
main.runCombinedStress.func1()
    .../forktest/child/main.go:225 +0x50
```

Line 88 is the large `make([]byte, …)` allocation in `runMmapStress` (see [`userspace/forktest/child/main.go`](../../userspace/forktest/child/main.go)).

**2. Parent (`forktest_parent`) — Pattern 2 (same session, second fault)**

```
SIGSEGV: segmentation violation
PC=0x13060 m=0 sigcode=1 addr=0x1e251f000

goroutine 1 gp=0x1e00021c0 m=0 mp=0x1edc40 [syscall]:
syscall.Syscall(0x3f, 0x4, 0x1e0087718, 0x400)   // read(fd=4, buf, 1024)
golang.org/x/sys/unix.Read(...)
main.main()
    .../forktest/parent/main.go:176 +0xd40
```

`0x3f` is **63** decimal = **`read`** on Linux arm64. The buffer pointer `0x1e0087718` is a normal-looking stack/local slot; the reported fault **`addr=0x1e251f000`** is in the **~0x1e0…** Go heap range (unlike **`0x2`** in the child). Register dump included `r0=0x4` (fd), consistent with draining the child's stdout pipe in the epoll loop.

**3. `ps` after the crash**

The first `ps` may list **many rows** with the same `/bin/forktest_child … -combined_stress` cmdline and odd **PPID chains** (e.g. threads under one child). That matches **goroutine / runtime threads** (`CLONE_VM`) each appearing as a schedulable entity in Akuma’s process listing. A **second** `ps` in the same session showed **no processes** — everything had exited after the faults.

**4. Build paths in the traceback**

Paths such as `/opt/homebrew/Cellar/go/1.25.5/...` are from the **host** used to build the `GOOS=linux GOARCH=arm64` binary; the panic still occurred on the **Akuma** target.

**5. Delayed full kernel freeze (anecdotal, same session)**

Sometime **after** the user-level panic/`SIGSEGV` sequence above, the **whole guest** appeared to **lock up** (e.g. SSH stopped responding). That was **not** in the same snippet as the forktest traceback, so it is **not** proven causal—only **temporally** related.

If this repeats, capture **serial / QEMU log** from the freeze window and look for: a thread stuck **forever** in a spinlock (ext2, process table, lazy-region, or fault path), **interrupts masked** too long, or **memory corruption** from an earlier fault that only manifests when another subsystem runs. Until there is a trace, treat “freeze after forktest” as an open **follow-on** symptom tied to the same stress scenario, not a separately root-caused bug.

## The `0x2` Value

The value `0x2` is suspicious because:

1. It's too small to be a valid heap pointer (Go heap starts at ~0x1e000_0000)
2. It's not NULL (0x0) which would indicate a clear nil pointer
3. It appears in **child** processes in these traces; the **parent** sometimes faults at a **heap-like** address (e.g. `0x1e251f000`) instead — see [Captured SSH log](#captured-ssh-log-2026-04-14)
4. It suggests corruption of Go's span/mheap data structures

Possible sources of `0x2`:
- A corrupted `mspan.base` pointer
- A freed/recycled span that still contains stale metadata
- A race condition leaving partial pointer values

## Verified Non-Issues

### clock_gettime Syscall

The `[EFAULT] nr=113` log entry appearing before crashes was investigated. Analysis showed:
- The args `[0x1e0a7aff0, 0x4fc0, ...]` indicate garbage register state, not a real syscall
- `clock_gettime` implementation is Linux-compatible (verified with 8 kernel tests)
- The EFAULT was from boot tests, not runtime crashes

Tests added:
- `test_clock_gettime_struct_layout` - Verifies `struct timespec` matches Linux ABI
- `test_clock_gettime_realtime` - CLOCK_REALTIME returns valid time
- `test_clock_gettime_monotonic` - CLOCK_MONOTONIC never goes backwards
- `test_clock_gettime_all_clock_ids` - All clock IDs accepted
- `test_clock_gettime_efault_null_ptr` - NULL pointer returns EFAULT
- `test_clock_gettime_efault_invalid_ptr` - Invalid pointer returns EFAULT
- `test_clock_getres_basic` - Resolution query works
- `test_clock_getres_null_ptr` - NULL res pointer allowed (Linux compat)

### Sigaltstack Handling

Sigaltstack handling was verified:
- `clone_thread` creates new M-threads with `alt_sp=0x0` (correct)
- Forked processes inherit sigaltstack from parent (correct for fork semantics)
- SIGURG guard in `entry_point_trampoline` clears pending signals for uninitialized threads

## Theories to Investigate

### Theory 1: CoW Page Fault Race Condition

**Hypothesis**: When multiple Go M-threads fault on the same CoW page simultaneously, the page fault handler may corrupt allocator metadata.

**Evidence**:
- Crashes occur in multi-threaded Go processes
- `CLONE_VM` threads share address space
- Go's heap spans cross page boundaries

**Investigation steps**:
1. Add logging to `handle_cow_fault()` when Go heap pages are duplicated
2. Check for lock contention in CoW fault handling
3. Verify TLB invalidation is correct for all CPUs/threads

### Theory 2: Demand Paging Race in Lazy Regions

**Hypothesis**: The `LAZY_REGION_TABLE` operations have a race condition when multiple threads fault on the same lazy region.

**Evidence**:
- Go allocates large lazy regions (e.g., `mmap 0x6400000` = 100MB)
- Multiple M-threads can fault on different pages within the same region
- The region lookup and physical page allocation may not be fully atomic

**Investigation steps**:
1. Add per-region locks for demand paging
2. Log when two threads fault on the same region simultaneously
3. Verify physical page is correctly mapped for all faulting threads

### Theory 3: Process/Thread Address Space Confusion

**Hypothesis**: With `CLONE_VM` threads, the address-space owner PID tracking has edge cases that cause wrong page tables to be used.

**Evidence**:
- Lazy regions are keyed by "address-space owner PID"
- Thread groups share address space via `CLONE_VM`
- The `find_process_info_page_owner` function may return wrong PID in some cases

**Investigation steps**:
1. Log PID used for lazy region lookups vs actual thread's PID
2. Verify TTBR0 (page table base) is consistent across all threads in a group
3. Check if terminated threads' PIDs are incorrectly reused

### Theory 4: OOM Handling Corrupts Allocator State

**Hypothesis**: When physical memory runs low (OOM), the kernel's error handling corrupts Go's heap state.

**Evidence**:
- With 256MB RAM, `[DA-DP] ... anon alloc failed, 0 free pages` appears
- OOM handling may return error codes that Go misinterprets as pointers
- Even with 2GB RAM, memory pressure from multiple children could trigger edge cases

**Investigation steps**:
1. Run with `MEMORY=4096M` to eliminate OOM entirely
2. Add logging when demand paging fails due to OOM
3. Verify mmap failure returns correct -ENOMEM to userspace

### Theory 5: Signal Delivery During Allocation

**Hypothesis**: SIGURG for goroutine preemption arrives during `mallocgc` critical section, corrupting allocator state.

**Evidence**:
- Go sends SIGURG to M-threads for preemption
- `mallocgc` is complex with multiple internal data structures
- Go's allocator should be signal-safe since Go 1.14, but kernel-level signal delivery differs

**Investigation steps**:
1. Log all SIGURG deliveries with PC at delivery time
2. Check if any SIGURG arrives while PC is in `mallocgc` range
3. Test with `GODEBUG=asyncpreemptoff=1` to disable preemption signals

## Diagnostic Commands

### Check kernel logs for crashes:
```bash
grep -E "DA-MISS|WILD-DA|SIGSEGV-HEAP" /tmp/akuma_output.txt
```

### Check thread creation:
```bash
grep -E "clone_thread|TRAMP.*alt_sp" /tmp/akuma_output.txt
```

### Check memory state:
```bash
grep -E "DA-DP|anon alloc failed|free pages" /tmp/akuma_output.txt
```

### Check signal delivery:
```bash
grep -E "signal.*deliver|tkill.*sig=23" /tmp/akuma_output.txt
```

## Files of Interest

| File | Purpose |
|------|---------|
| `crates/akuma-exec/src/process/mod.rs` | `fork_process`, `clone_thread`, `entry_point_trampoline` |
| `crates/akuma-exec/src/mmu/mod.rs` | Address space management, CoW handling |
| `src/exceptions.rs` | Page fault handling, signal delivery |
| `src/pmm.rs` | Physical memory manager, CoW reference counting |
| `crates/akuma-exec/src/threading/mod.rs` | Thread state, sigaltstack, pending signals |

## Test Isolation Ideas

1. **Single M-thread**: Run with `GOMAXPROCS=1` - if crashes disappear, confirms multi-threading issue

2. **No forking**: Run child directly without fork - isolates fork/CoW from thread creation

3. **Simple allocations**: Modify `runMmapStress` to use smaller allocations - checks if large allocations trigger the bug

4. **Disable preemption**: Build Go binary with `GODEBUG=asyncpreemptoff=1` - eliminates SIGURG as a factor

## Summary

As of **2026-04-14**, both crash patterns below **still occur** on real runs; they are **not** fixed by filesystem-only changes.

Both crash patterns show `addr=0x2` which indicates Go's heap allocator is returning corrupted pointers. The corruption likely originates from:

1. A race condition in the kernel's handling of shared memory (CoW or demand paging)
2. Incorrect address-space management for `CLONE_VM` thread groups
3. Signal delivery timing issues during allocation

A **delayed full kernel freeze** after the same kind of forktest run has been observed once (see [§Captured SSH log](#captured-ssh-log-2026-04-14) item 5); it is **not** yet tied to a specific kernel stack without serial/QEMU logs.

The most promising investigation paths are:
1. Add locking/logging to CoW fault handler
2. Verify `LAZY_REGION_TABLE` operations are atomic
3. Test with `GOMAXPROCS=1` to confirm multi-threading involvement
