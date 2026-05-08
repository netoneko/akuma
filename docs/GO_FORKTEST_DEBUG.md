# Go Forktest Crash Analysis

This document details crash patterns seen when running `forktest_parent` with **stress flags** (especially **`-combined_stress`**, **`-mmap_test`**, or **`-file_io`**) on Akuma OS. The **child** often shows `addr=0x2` in Go's allocator; the **parent** can fault in **`read()`** on the epoll pipe with a **heap-range** fault address; **`-file_io`** can also contribute to **deadlocks** (guest or SSH) via temp-file traffic on ext2 (see [Isolation matrix](#isolation-matrix-2026-04-14)).

## Current status (2026-05-07, updated 2026-05-08)

**Forktest mmap stress still reproduces intermittent allocator crashes** (`pc≈0x86768`, `runMmapStress` / `memclr`), with fault addresses varying over time (`0x2`, **`0x10`** / **`0x12`** (low canonical VAs), negative “errno-like” FARs, etc.). A **lazy-region / `tgid` owner fix** (2026-05-07) addresses a real **`EFAULT` / wrong-owner** class of bugs but **does not close** the **SIGURG + syscall / JIT replay** failure mode seen in serial evidence (see **`crash5.log`** below).

**2026-05-08:** Pure-C **`mmap_stress`** ([`userspace/forktest/c_stress/`](../userspace/forktest/c_stress/)) can run cleanly **standalone**, but **`forktest_parent --use_c_child`** still kills the **Go parent** in **Pattern 2** (`read` / epoll pipe). Serial **`crash14.log`** shows **`mmap`** (**`nr=222`**) from a C child with **`x0 = −22`** (errno-shaped **before** kernel handling)—see [§E](#e-pure-c-mmap_stress--crash14-serial-2026-05-08) below. That points to **syscall-entry / trap-frame corruption**, not Go’s allocator alone.

This failure mode is **orthogonal** to ext2 fixes that removed spurious **`input/output error`** on `/tmp` under load (blocking `read_state()` and a single `write_state()` for `write_at` in [`crates/akuma-ext2/src/ext2.rs`](../crates/akuma-ext2/src/ext2.rs)). If you see **EIO** on temp files, that was filesystem contention; if you see **`addr=0x2`** / **`0x10`** in the Go allocator, treat it as the **heap + demand paging + signal / syscall-frame** investigation described below.

| What you see | Likely bucket | Where to read |
|--------------|----------------|---------------|
| `write /tmp/...: input/output error` | ext2 read path starved / `IoError` | ext2 history in `GO_FORK_EXEC_FIXES.md` |
| `addr=0x2`, `0x10`, `0x12`, panic in `mallocgc` / `memclr` | Bad pointer / span base in Go after kernel user-mode fault path | This file, §Crash Pattern 1–2, **`crash5.log`** |
| **`unexpected fault address 0xfffffffffffffffa`** (`-6`), **`fatal error: fault`** | Prior **`clock_gettime` → EINVAL (-22)**; FAR **`-6`** = **`-22+16`** (errno misused as base + `sizeof(timespec)`) | **`crash4.log`** / § Serial captures |
| **`unexpected fault address 0xffffffffffffffb0`**, `fatal error: fault` in `memclr` | Same **`pc≈0x86768`** family; often at **50–70 MiB** `-mmap_alloc_mb` | [Empirical threshold](#empirical-allocation-threshold-2026-04-14-session) |
| `[JIT] IC flush + replay … bogus nr=…` near fault | Stale / corrupted syscall dispatch state around SVC replay | [`src/exceptions.rs`](../src/exceptions.rs), `FIX_MEMORY_MAPPING.md`, `EPOLL_PERFORMANCE.md` |
| **`[EINVAL] nr=222`** (`mmap`), **`args[0]`** = **`0xffffffffffffffea`** (−22) | Same **errno-as-GPR** family as xattr/`clock_gettime`; **`mmap` addr** must never be an errno word | **`crash14.log`**, §E; [`src/syscall/mod.rs`](../src/syscall/mod.rs) `nr::MMAP` |
| SSH **disconnect** after commands finish | Often **client / router idle timeout**; confirm with serial still running | Not proven guest deadlock from **`crash5.log`** alone |

**Mitigations while debugging:** ample RAM (`MEMORY=2048M` or higher), `GODEBUG=asyncpreemptoff=1`, or avoid **`-mmap_test`**, **`-combined_stress`**, and **`-file_io`** until fixed. **`GOMAXPROCS=1` does not prevent** the **parent** `read()` SIGSEGV when **`-mmap_test`** is enabled ([Isolation matrix](#isolation-matrix-2026-04-14)).

## Isolation matrix (2026-04-14)

Shell: `export GOMAXPROCS=1` for all runs below. Command line: `forktest_parent --duration 10s` plus flags.

| Child mode | Outcome |
|------------|---------|
| **(none)** — children run default main (no stress) | **Stable.** Parent sends SIGTERM at deadline; `Wait()` reports `signal: killed`; empty child stdout. |
| **`-mmap_test`** | **Parent SIGSEGV** in `unix.Read` on pipe (**`main.go:199`**): `PC≈0x13060`, fault `addr≈0x1e39df000`, `syscall` read `fd=4`. Same shape as [Pattern 2](#crash-pattern-2-parent-process-heap-corruption). **Does not require** `-combined_stress` or multiple Go M-threads in the parent. |
| **`--use_c_child`** + **`-mmap_test`** | Same **parent** failure as above (Go parent + epoll **`read`**); children run **`/bin/mmap_stress`** instead of Go. **C children can still `exit_group code=0`** after the parent dies — not proof the parent path is sound. **`crash14.log`**: **`nr=222`** (`mmap`) with errno-shaped **`x0`** from a child. See §E. |
| **`-file_io`** | **Not a safe mode.** One short run showed children printing `Received terminated, exiting gracefully.` before kill, but **`-file_io` has also reproduced a full deadlock** (no forward progress in SSH / shell). That lines up with **concurrent temp-file writes** on ext2 and earlier **`ps`** / shell hangs under I/O stress—count **`file_io`** as a **deadlock risk**, not “stable”. |
| **`-send_signal`** | **Stable** (benign race): `Failed to send SIGINT … process already finished` if the child exits before 500 ms; then deadline kill as usual. |

**Conclusion:** The **mmap heap stress in children** (`runMmapStress`, large `make([]byte, …)`, or pure-C **`mmap_stress`**) is enough to trigger the **parent `read()` SIGSEGV**; **`GOMAXPROCS=1` does not rule out “multi-M in parent”** as the sole cause—it rules out **parent** multi-threading as required for that crash. Replacing Go children with **C `mmap_stress`** does **not** eliminate the parent crash (§E). Separately, **`file_io`** stress can **deadlock** the system even when mmap does not SIGSEGV the parent—likely **filesystem / lock / scheduler** interaction, not only Go’s allocator.

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

**Static tests:** `clock_gettime` / `clock_getres` behave plausibly Linux-compat in isolation (see tests below).

**Runtime (2026-05-07):** Serial logs show **`nr=113`** with **pointer-sized “clock_id”** in **`x0`** and small garbage in **`x1`**, often **`tkill(…, SIGURG)`** (async preempt) nearby. That is **not** explained by the syscall implementation alone; it implicates **trap-frame / syscall-restart / signal** interaction. Returning **strict `EINVAL`** for oversized `clock_id` avoids EL1 `copy_to_user` on a bogus “second arg” pointer, but leaves **`-22` in `x0`**, which Go can still misuse (see **`crash4.log`**: FAR **`0xfffffffffffffffa`** = **`-22+16`**).

**Rejected mitigation:** Treating oversized **`x0`** as the timespec pointer and writing 16 bytes (**“recover x0 as tp”**) was **abandoned**: **`crash5.log`** shows **`SIGURG`** → that path → **`[WILD-DA] FAR=0x`10` ELR=0x86768`** with **`x0=0x0`** at fault time (nil-adjacent **`memclr`**), i.e. no durable fix and possible heap clobber.

The older note that some **`[EFAULT] nr=113`** lines come from **kernel self-tests at boot** remains true; **forktest** sessions additionally show **runtime** **`nr=113`** adjacent to allocator faults.

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

1. **Single M-thread (`GOMAXPROCS=1`)**: **Tried (2026-04-14)** — parent still **SIGSEGV** in `read()` with **`-mmap_test`** only. This **does not** point to parent goroutine count alone; keep it for narrowing **child** threading vs allocator.

2. **No forking**: Run child directly without fork - isolates fork/CoW from thread creation

3. **Smaller allocations**: **`forktest_parent`** / **`forktest_child`** accept **`-mmap_alloc_mb=N`** (default **100**) to scale lazy region size without editing Go source (e.g. **`-mmap_alloc_mb=4`** with **`-num_children=1`**).

4. **Disable preemption**: `GODEBUG=asyncpreemptoff=1` - eliminates SIGURG as a factor (still worth testing; not yet logged as definitive)

## Appendix: mmap / demand-paging investigation (implementation notes)

### Serial capture and grep

- **Script:** [`scripts/capture_serial_forktest_mmap.sh`](../scripts/capture_serial_forktest_mmap.sh) — runs [`scripts/run.sh`](../scripts/run.sh) with **`tee`** to **`full.log`** (or the path you pass). Set **`MEMORY=2048M`** (default in script) as needed.
- **Manual:** `MEMORY=2048M ./scripts/run.sh 2>&1 | tee full.log`
- **After a forktest repro over SSH**, search the log for demand paging and faults:

```bash
rg '\[mmap\]|\[DA-MISS\]|\[DA-DP\]|\[WILD-DA\]|\[Fault\]|\[JIT\]|nr=113|exit_group|forktest|mmap_alloc_mb|clock_gettime' full.log
```

Correlate **`pid=`** / **`ppid=`** in **`[DA-MISS]`** lines with **`[exit_group]`** PIDs to tie faults to parent vs child address spaces.

### Kernel audit: owner PID and lazy regions (read-path checklist)

Code review (no behavioral change required for this appendix):

| Mechanism | Location | Notes |
|-----------|----------|--------|
| Thread-group owner for faults | [`crates/akuma-exec/src/process/children.rs`](../crates/akuma-exec/src/process/children.rs) | **`address_space_owner_pid_for_fault()`** uses **`current_process().tgid`**. |
| Lazy region lookup for faults | Same file | **`lazy_region_lookup_for_page_fault(pid, va)`** tries **`pid`**, then the owner PID if different — aligns **`LAZY_REGION_TABLE`** with **`CLONE_VM`**. |
| EL0 demand paging / CoW | [`src/exceptions.rs`](../src/exceptions.rs) | **`as_owner`** from **`address_space_owner_pid_for_fault()`**; **`fault_mutex`** / **`DaFaultGuard`** on **`as_owner`**; **`lazy_region_lookup_for_page_fault(pid, far)`** for translation faults. |
| **`sys_mmap` lazy policy** | [`src/syscall/mem.rs`](../src/syscall/mem.rs) | Large anonymous maps use **lazy** (`pages > 256`, **`MAP_NORESERVE`**, etc.). |

Regression coverage includes [`src/process_tests.rs`](../src/process_tests.rs) (**`test_lazy_region_lookup_for_page_fault_clone`**, **`test_kill_thread_group_preserves_lazy_regions`**, etc.).

### Branch: parent **`read()` SIGSEGV** vs child allocator

If serial shows **no** suspicious **`[DA-MISS]`** / **`[WILD-DA]`** on children but the **parent** still faults in **`unix.Read`** (**[`userspace/forktest/parent/main.go`](../userspace/forktest/parent/main.go)** pipe drain), treat **pipe + syscall return** separately:

| Layer | Files |
|-------|--------|
| **`read` syscall** | [`src/syscall/fs.rs`](../src/syscall/fs.rs) — **`sys_read`** |
| **Pipe buffers** | [`src/syscall/pipe.rs`](../src/syscall/pipe.rs) — **`pipe_read`**, **`pipe_write`**, waiters |

### `mmap_alloc_mb` flag

- **Parent:** **`forktest_parent -mmap_alloc_mb=N`** (forwarded when **`-mmap_test`** or **`-combined_stress`**).
- **Child:** **`forktest_child -mmap_alloc_mb=N`** (used by **`runMmapStress`**). Lower values reduce lazy region size and fault volume for bisection.

### Empirical allocation threshold (2026-04-14 session)

Conditions: **`export GOMAXPROCS=1`**, **`forktest_parent --duration 10s -mmap_test -mmap_alloc_mb=N`** (default **3 children**). Outcomes below are from **SSH transcripts**; for kernel-side correlation, capture **[`full.log`](../../full.log)** (or another serial path) and use the [grep patterns above](#serial-capture-and-grep).

| `-mmap_alloc_mb` | Typical outcome |
|------------------|-----------------|
| **10** | **Stable** — children **`Received terminated, exiting gracefully`**, parent completes. Reproduced on repeated runs. |
| **50** | **Mixed.** Often stable like 10 MB, but the **same** command also produced **`fatal error: fault`** / **`unexpected fault address 0xffffffffffffffb0`** in **`memclrNoHeapPointers`** → **`mallocgcLarge`** (request size **`0x3200000`** = 50 MiB). Treat as **non-deterministic** at this size. |
| **70** | **Fails** — panics with **`addr=0x2`** or **`addr=0x12`**, and **`0xffffffffffffffb0`** in **`memclrNoHeapPointers`** (same **`pc≈0x86768`** family). |
| **100** | **Fails** — **`addr=0x2`** at **`pc=0x86768`**, **`runMmapStress`** / goroutine 1 (duplicate panics when multiple children hit the bug). |

**Interpretation:** Failures cluster **above ~50–70 MiB per slice** (not exact; **50 MiB can still pass or fail**). Guest **`free`** during the session still showed **~1.6 GB RAM free**, so these are **not** simple OOM from the shell’s view—favor **kernel demand-paging / lazy region / allocator visibility** bugs over “out of memory” until serial **`[DA-DP]`** / **`[DA-MISS]`** proves otherwise.

**Fault address `0xffffffffffffffb0`:** Appears in Go’s **`fatal error: fault`** path while **`memclr`** runs on a large allocation. It is a distinct pattern from **`0x2`** but the **same code site** (`memclr_arm64.s` / **`mallocgcLarge`**). Log both when filing kernel issues.

### `full.log` correlation (serial grep, 2026-04-14)

Captured **[`full.log`](../../full.log)** (~48k lines) from the same session as the table above. Useful command:

```bash
rg -n 'forktest|DA-MISS|WILD-DA|mmap_alloc_mb' full.log
```

**Stable runs** ( **`exit_group` … `forktest_child` `code=0`** , parent `code=0` ):

- **`mmap_alloc_mb=10`**: e.g. execve lines ~2147–2296; exits ~8348–8412.
- **`mmap_alloc_mb=50`** (first batch): ~8587–8718 execve; ~14813–14874 clean exit.

**Failing `mmap_alloc_mb=100`** (children **`exit_group` `code=2`**):

- **`[DA-MISS] pid=137 … va=0x2`** (and pid **138** similarly) → **`[DP] no lazy region for FAR=0x2`** → **`[WILD-DA] pid=137 FAR=0x2 ELR=0x86768`** , **`last_sc=18446744073709551615`** (no syscall active).
- Immediately **before** the first **`[DA-MISS]`** on pid **137**, the log shows **`[EFAULT] nr=113`** (**`clock_gettime`**) with odd args, an **EL1 sync** (**`EC=0x25`**) with **“Kernel accessing user-space address”**, and **`WARNING: … stale TTBR0`**. That lines up with the older “garbage **`clock_gettime`**” note in [Verified Non-Issues](#clock_gettime-syscall) but here it is **adjacent to** the **`FAR=0x2`** wild fault—worth treating as **noise vs cause** only after more runs.

**Failing `mmap_alloc_mb=70`**:

- **`FAR=0x12`** and **`FAR=0xffffffffffffffb0`** on different PIDs; **`[WILD-DA]`** still reports **`ELR=0x86768`**.
- For **`FAR=0xffffffffffffffb0`**, the kernel prints **`*** FAR=0xffffffffffffffb0 is -80 (???) - syscall error used as pointer! ***`** (signed **`FAR == -80`**). Register dumps show **`x0`** values like **`0xffffffffffffffa0`** (another errno-like pattern). Demand-paging path still logs **`[DP] no lazy region for FAR=…`** before **`[WILD-DA]`**.

**Second `mmap_alloc_mb=50`** run (one child faults):

- Same **`FAR=0xffffffffffffffb0`** / **`ELR=0x86768`** chain for pid **205** (~43914–43932).

**Takeaway:** Serial proves the “bad address” is delivered to the fault handler as **`FAR`** on a **demand-paging / lazy miss** path (**`[DA-MISS]`** → **`no lazy region`** → **`[WILD-DA]`**), not only as a Go-side panic string. The PC **`0x86768`** matches userspace **`memclr`**. Next kernel step: determine **why `ELR`/`FAR` pair** ends up as **`0x2`** / **`-80` sign-extended** (bad **`mmap` return**, bad **`brk`**, or **corrupted register state** across syscall) while **`[DP]`** correctly reports **no** lazy mapping for that bogus VA.

## Summary & status (2026-05-07)

### A. `EFAULT` / lazy-region owner (`tgid`) — fixed class

Many `addr=0x2`-style panics were consistent with the kernel returning **`EFAULT` (`-14`)** from syscalls that touched **lazy-mapped Go heap** while **`lazy_region_lookup` used `read_current_pid()`** instead of the **thread-group owner (`tgid`)**. Under **`CLONE_VM`**, lookup failed, demand paging was skipped, and **`copy_to_user` / validation** failed → **`EFAULT`** → Go mis-handled the negative value as an address (including **`FAR = -14 + 16`** patterns).

**Fix (landed):** resolve the address-space owner for lazy regions and mmap metadata (`address_space_owner_pid_for_fault` / **`tgid`**) in **`lazy_region_lookup`**, **`sys_mmap`**, **`sys_mremap`**, **`sys_munmap`**, **`sys_mprotect`**, **`sys_madvise`**, etc., so worker threads don’t strand lazy regions or physical pages on the wrong `Process`.

### B. OOM / `user_frames` leak on `CLONE_VM` workers — fixed class

Heavy **`forktest`** also produced **`[DA-DP] … anon alloc failed, 0 free pages`** when **worker threads** mapped memory into per-thread `UserAddressSpace` slices that **skipped freeing `user_frames` on `drop` for `shared` address spaces**. Routing mappings through the owner **`tgid`** aligns teardown with Linux-like sharing semantics.

### C. `clock_gettime` / SIGURG / syscall frame — **open**

**`crash4.log` (serial):** After rejecting pointer-like **`x0`** with **`EINVAL`**, user-visible **`unexpected fault address 0xfffffffffffffffa`**: kernel register dump shows **`x0 = -22` (`EINVAL`)** and **`FAR = -6`** (**`-22 + sizeof(timespec)`** on AArch64), i.e. **errno in `x0` treated as a base pointer** inside Go’s runtime.

**`crash5.log` (serial, `MEMORY=2048M`, `-mmap_alloc_mb=70`):**

- First failing child sequence (**kernel with H4 recovery; reverted after this capture**): **`tkill(…, sig=23)`** (**`SIGURG`**) → NDJSON **`clock_gettime_recover_x0_as_tp`** (**hypothesis `H4`**, `x0≈0x1e…` heap) → **`[DA-MISS] va=0x10`**, **`[WILD-DA] pid=95 FAR=0x10 ELR=0x86768`**, fault **`pc` matches `memclr`**, register dump **`x0=0x0`**.
- Second run (another child): **`[JIT] IC flush + replay #1 bogus nr=8053579784 ELR=0x86768`** then **`tkill(…, sig=23)`** → **`FAR=0x12`**, same **`ELR`**, **`exit_group` `code=2`**. No **`H4`** line required for that path.

**Conclusion:** **`clock_gettime`-only heuristics do not resolve forktest**; **`recover x0 as tp` was removed** from [`src/syscall/time.rs`](../src/syscall/time.rs) after **`crash5`**. Remaining work aligns with **async preemption (`SIGURG`)**, **SVC / JIT IC flush replay**, and **signal return** correctness (see [`src/exceptions.rs`](../src/exceptions.rs), [`FIX_MEMORY_MAPPING.md`](FIX_MEMORY_MAPPING.md), [`EPOLL_PERFORMANCE.md`](EPOLL_PERFORMANCE.md)).

### D. SSH “hang” / disconnect

**`crash5.log`** ends with **`forktest_parent` / children `exit_group` `code=0`**. An SSH **“Connection closed”** afterward is **not** demonstrated as a guest deadlock from this capture alone—check **host timeouts** and **parallel serial tee**.

### E. Pure-C `mmap_stress` + `crash14` serial (2026-05-08)

**Motivation:** Swap **`forktest_child`** (Go) for **`/bin/mmap_stress`** (static musl: **`mmap` → `memset` → `munmap`**, see [`userspace/forktest/c_stress/README.md`](../userspace/forktest/c_stress/README.md)) via **`forktest_parent --use_c_child`**, to see whether crashes are **only** Go’s heap/runtime.

**Results:**

1. **`mmap_stress` alone** (no parent farm): can finish many iterations and **`exit_group code=0`** — the **anonymous mmap demand-paging path is not unconditionally broken** for a single process.

2. **`forktest_parent --use_c_child --duration 10s --mmap_test=true --mmap_alloc_mb=70`:** the **parent still crashes** in **[Pattern 2](#crash-pattern-2-parent-process-heap-corruption)** — **`unix.Read`** on the epoll pipe (**`PC≈0x13060`**), fault address in the **heap VA range** (e.g. **`0x13d96000`**). Only children were swapped to C; **parent remains Go + epoll + pipe `read`**.

3. **`crash14.log`** (serial, same style of run):

   - **`forktest_parent`** exits **`exit_group … code=2`** shortly after **`[signal] deliver sig=11`** with **`fault_pc=0x13060`** (**SIGSEGV** on the parent thread draining pipes — matches userspace **`read`** trampoline).

   - **Immediately before** that, kernel prints **`[EINVAL] nr=222 pid=<child>`** with **`args=[0xffffffffffffffea, …]`**. **`nr::MMAP` is 222** in [`src/syscall/mod.rs`](../src/syscall/mod.rs). **`0xffffffffffffffea`** is **`−22` (`EINVAL`)** in **`x0`** — the **first argument to `mmap`** (address hint). The C source only ever passes **`NULL`**; this implies **GPR corruption at syscall entry** (or a logging/TID mix-up—either way, investigate trap path).

   - **After** the parent dies, **`mmap_stress`** children often **`exit_group code=0`** — they can **outlive** the parent until **`SIGTERM`** / duration; **child `code=0` does not prove** the parent **`read`** path is safe.

**Interpretation:** Parallel **mmap storm + parent pipe I/O** correlates with **errno-shaped words appearing in syscall argument registers** — seen on **`crash13`** (**xattr / `clock_gettime`**) and now on **`mmap`** in **`crash14`**. That strengthens the hypothesis that **kernel-side syscall / trap-frame / `clone` / `rt_sigreturn` plumbing** must be audited—not only Go’s **`mallocgc`**.

**Mitigations while debugging:** `GODEBUG=asyncpreemptoff=1`, ample RAM, smaller `-mmap_alloc_mb` for bisection; reduce contention with **`--num_children=1`** when isolating parent vs child behavior.
