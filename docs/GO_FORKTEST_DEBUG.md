# Go Forktest Crash Analysis

This document details crash patterns seen when running `forktest_parent` with **stress flags** (especially **`-combined_stress`**, **`-mmap_test`**, or **`-file_io`**) on Akuma OS. The **child** often shows `addr=0x2` in Go's allocator; the **parent** can fault in **`read()`** on the epoll pipe with a **heap-range** fault address; **`-file_io`** can also contribute to **deadlocks** (guest or SSH) via temp-file traffic on ext2 (see [Isolation matrix](#isolation-matrix-2026-04-14)).

## Current status (2026-05-07, updated 2026-05-08 — interim conclusions below)

**Forktest mmap stress still reproduces intermittent allocator crashes** (`pc≈0x86768`, `runMmapStress` / `memclr`), with fault addresses varying over time (`0x2`, **`0x10`** / **`0x12`** (low canonical VAs), negative “errno-like” FARs, etc.). A **lazy-region / `tgid` owner fix** (2026-05-07) addresses a real **`EFAULT` / wrong-owner** class of bugs but **does not close** the **SIGURG + syscall / JIT replay** failure mode seen in serial evidence (see **`crash5.log`** below).

**2026-05-08:** Pure-C **`mmap_stress`** ([`userspace/forktest/c_stress/`](../userspace/forktest/c_stress/)) can run cleanly **standalone**, but **`forktest_parent --use_c_child`** still kills the **Go parent** in **Pattern 2** (`read` / epoll pipe). Serial **`crash14.log`** shows **`mmap`** (**`nr=222`**) from a C child with **`x0 = −22`** (errno-shaped **before** kernel handling)—see [§E](#e-pure-c-mmap_stress--crash14-serial-2026-05-08) below. That points to **syscall-entry / trap-frame corruption**, not Go’s allocator alone.

**Kernel instrumentation (2026-05-08):** When **`[EINVAL]` / `[EFAULT]` / `[ENOSYS]`** fire, serial can include **`tid=`**, **`ELR=`**, full **`x0`–`x5`**, and for **`nr=222`** a follow-on **`[mmap-einval]`** line with **`reason=`** and decoded **`flags=`**—see [Serial errno diagnostics](#serial-errno-diagnostics-2026-05-08). Toggle via **`SYSCALL_ERRNO_DIAG_EXTRA`** in [`src/config.rs`](../src/config.rs). Regression tests: **`mmap_fixed_addr_unaligned_einval_helper`**, **`mmap_einval_through_handle_syscall`** in [`src/tests.rs`](../src/tests.rs).

**Pipe `read` tracing (2026-05-08):** With **`SYSCALL_DEBUG_PIPE_READ`** (and optional **`SYSCALL_DEBUG_PIPE_READ_SAMPLE`**) in [`src/config.rs`](../src/config.rs), serial emits **`[pipe-read] enter`** / **`EFAULT`** lines from [`src/syscall/fs.rs`](../src/syscall/fs.rs) for **`PipeRead`** FDs—correlate with **`fault_pc≈0x13060`** via **`tprint`** timestamps. [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh) greps **`[pipe-read]`** alongside mmap / fault markers.

Synthesis of evidence so far: [§ Interim conclusions](#interim-conclusions-2026-05-08). **Continuing work:** [§ Agent handoff](#agent-handoff-2026-05-08).

This failure mode is **orthogonal** to ext2 fixes that removed spurious **`input/output error`** on `/tmp` under load (blocking `read_state()` and a single `write_state()` for `write_at` in [`crates/akuma-ext2/src/ext2.rs`](../crates/akuma-ext2/src/ext2.rs)). If you see **EIO** on temp files, that was filesystem contention; if you see **`addr=0x2`** / **`0x10`** in the Go allocator, treat it as the **heap + demand paging + signal / syscall-frame** investigation described below.

| What you see | Likely bucket | Where to read |
|--------------|----------------|---------------|
| `write /tmp/...: input/output error` | ext2 read path starved / `IoError` | ext2 history in `GO_FORK_EXEC_FIXES.md` |
| `addr=0x2`, `0x10`, `0x12`, panic in `mallocgc` / `memclr` | Bad pointer / span base in Go after kernel user-mode fault path | This file, §Crash Pattern 1–2, **`crash5.log`** |
| **`unexpected fault address 0xfffffffffffffffa`** (`-6`), **`fatal error: fault`** | Prior **`clock_gettime` → EINVAL (-22)**; FAR **`-6`** = **`-22+16`** (errno misused as base + `sizeof(timespec)`) | **`crash4.log`** / § Serial captures |
| **`unexpected fault address 0xffffffffffffffb0`**, `fatal error: fault` in `memclr` | Same **`pc≈0x86768`** family; often at **50–70 MiB** `-mmap_alloc_mb` | [Empirical threshold](#empirical-allocation-threshold-2026-04-14-session) |
| `[JIT] IC flush + replay … bogus nr=…` near fault | Stale / corrupted syscall dispatch state around SVC replay | [`src/exceptions.rs`](../src/exceptions.rs), `FIX_MEMORY_MAPPING.md`, `EPOLL_PERFORMANCE.md` |
| **`[EINVAL] nr=222`** (`mmap`), **`args[0]`** = **`0xffffffffffffffea`** (−22) | Same **errno-as-GPR** family as xattr/`clock_gettime`; extended lines show **`reason=`** (`len==0`, **`fixed+unaligned`**, **`kernel_va`**, **`other`**)—see [Serial errno diagnostics](#serial-errno-diagnostics-2026-05-08) | **`crash14.log`**, §E; [`src/syscall/mod.rs`](../src/syscall/mod.rs); **`mmap_fixed_addr_unaligned_einval`** in [`src/syscall/mem.rs`](../src/syscall/mem.rs) |
| SSH **disconnect** after commands finish | Often **client / router idle timeout**; confirm with serial still running | Not proven guest deadlock from **`crash5.log`** alone |

**Mitigations while debugging:** ample RAM (`MEMORY=2048M` or higher), `GODEBUG=asyncpreemptoff=1`, or avoid **`-mmap_test`**, **`-combined_stress`**, and **`-file_io`** until fixed. **`GOMAXPROCS=1` does not prevent** the **parent** `read()` SIGSEGV when **`-mmap_test`** is enabled ([Isolation matrix](#isolation-matrix-2026-04-14)). **`asyncpreemptoff=1` alone does not reliably prevent Pattern 2**—see [§ Interim conclusions](#interim-conclusions-2026-05-08).

## Interim conclusions (2026-05-08)

These are **working hypotheses** from serial + SSH captures (**`crash16.log`**, **`crash14`**, same-session repros), not a closed root cause.

### 1. `GODEBUG=asyncpreemptoff=1` is necessary context, not a fix

In one session, **`forktest_parent --use_c_child …`** completed cleanly (**deadline SIGTERM**, children graceful, parent exited **0**), then a **second run** in the **same shell** (**`GODEBUG` still set**) **SIGSEGV**’d again (**`PC≈0x13060`**, **`read`** on a pipe FD). So **async goroutine preemption via `SIGURG` is not the only prerequisite** for Pattern 2; disabling it **reduces** urgency-signal traffic but **does not prove** the bug away. This aligns with the older finding that **`GOMAXPROCS=1` does not save the parent**.

### 2. Parent SIGSEGV: fault address vs `read` buffer

User registers at crash often show **`read(fd, buf, 0x400)`** with **`buf ≈ 0x1e0087708`**, while Go reports **`addr`** / **`fault`** around **`0x129a1000`**, **`0x1343f000`**, **`0x13d96000`**—**not** inside **`[buf, buf+0x400)`**. That **does not match** a naïve story “kernel **`copy_to_user`** faulted on the first byte of the pipe-read buffer.” It fits better **wrong effective address for some access while `PC` is still in the syscall/read trampoline** (e.g. **corrupted GPRs**) or **fault metadata** that does not map one-to-one to the **`copy_to_user`** range—still consistent with **syscall-entry / return / signal-frame** bugs rather than only “pipe implementation wrong.”

### 3. `SIGURG` at the same PC as `SIGSEGV` (serial **`crash16`**)

**`crash16.log`** shows **`SIGURG` (`sig=23`)** delivered to the parent thread with **`fault_pc=0x13060`** **before** the fatal **`SIGSEGV` (`sig=11`)** at the **same `fault_pc`**. That strengthened suspicion of **signal + syscall interaction**, but **§1** shows **`asyncpreemptoff`** does **not** eliminate the crash—so treat **`SIGURG`** as **correlated**, not **sufficiently causal** by itself.

### 4. Children: `mmap` **`[EINVAL]`** lines and **`[mmap-einval]`**

Extended logging shows **`nr=222`** with **`x1=0`** (**`len==0`**), **`x0`** sometimes a **prior mmap base** (e.g. **`0x10420000`**) or errno-shaped (**`0xffffffffffffffea`**), and **`prot` / `flags`** fields that are **not plausible Linux `mmap` arguments**. **`reason=len==0`** is accurate for the **`EINVAL`** return path; the **`flags=(FIXED|…)`** **decode can be meaningless** when **`x2`/`x3` are garbage**—do not read **`FIXED`** from the string alone. This supports **GPR corruption or reordering at SVC** under mmap load, independent of Go in the child.

### 5. Kernel implementation notes (landed in tree)

- **`sys_mmap`:** **`MAP_FIXED` / `MAP_FIXED_NOREPLACE`** **unaligned `addr` → `EINVAL`** is evaluated **before** **`lookup_process`** so **`handle_syscall`** from kernel-only tests and early rejects match **`EINVAL`**, not **`ESRCH`** (memory-test **`mmap_einval_through_handle_syscall`**).
- **Diagnostics:** **`SYSCALL_ERRNO_DIAG_EXTRA`**, **`SYSCALL_DEBUG_PIPE_READ`**, [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh).

### 6. Next narrowing steps

| Step | Purpose |
|------|---------|
| Serial **`grep`** **`[pipe-read]`** + **`fault_pc=0x13060`** + **`sig=11`** | See last **`buf=`** before crash; check for **`EFAULT copy_to_user`** vs fault without EFAULT. |
| **`--num_children=1`** | Reduce cross-process contention; if parent stabilizes, suspect **scheduler / concurrent mmap / FD** pressure. |
| Fresh process **vs** second run in same shell | Check for **accumulated kernel or libc state** (informal; serial **`pid=`** / **`pipe=`** ids still help). |
| **`[pipe-read]`** + **`[EINVAL] nr=222`** ordering | If syscall-arg garbage **clusters before** parent **`sig=11`**, prioritize **global trap / per-thread syscall state** audits over pipe-only fixes. |

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

For **`[EINVAL]`** / **`mmap`** correlation after the 2026-05-08 logging change, also search:

```bash
rg '\[EINVAL\]|\[EFAULT\]|\[ENOSYS\]|\[mmap-einval\]|mmap_fixed_addr|nr=222|tid=' full.log
```

Correlate **`pid=`** / **`ppid=`** in **`[DA-MISS]`** lines with **`[exit_group]`** PIDs to tie faults to parent vs child address spaces.

### Serial errno diagnostics (2026-05-08)

When a syscall returns **`EFAULT`**, **`ENOSYS`**, or **`EINVAL`**, the kernel may print an extended line (gated by **`SYSCALL_ERRNO_DIAG_EXTRA`** in [`src/config.rs`](../src/config.rs); default **`true`**, set **`false`** for the legacy short format).

**First line** (all dangerous errnos): **`[EINVAL]`** / **`[EFAULT]`** / **`[ENOSYS]`** with **`nr=<n> pid=<p> tid=<t> ELR=<pc-or-?> args=[x0…x5]`** — six AArch64 argument registers at SVC entry, plus the userspace program counter of the system call when the live trap frame is available (**`ELR=?`** if not). Thread id comes from the scheduler slot (**`current_thread_id`**).

**Second line** (only **`nr=222`** **`mmap`** returning **`EINVAL`**): **`[mmap-einval] reason=<token> addr=… len=… prot=… flags=0x…(FLAG|FLAG|…)`**. The **`reason`** token is derived from the syscall inputs using the same predicates as [`sys_mmap`](../src/syscall/mem.rs) (via **`mmap_fixed_addr_unaligned_einval`** and **`mmap_fixed_overlaps_kernel_va`**):

| **`reason=`** | Meaning |
|---------------|---------|
| **`len==0`** | **`x1`** was zero — **`EINVAL`** before address interpretation. |
| **`fixed+unaligned`** | **`MAP_FIXED`** or **`MAP_FIXED_NOREPLACE`** set and **`addr`** not page-aligned — **errno-shaped `addr`** (e.g. **`0xffffffffffffffea`**) produces **`EINVAL`** here without implying **`NULL` was corrupted** if **`MAP_FIXED`** is actually set in **`flags`**. |
| **`kernel_va`** | Fixed mapping would overlap the kernel identity-map VA window. |
| **`other`** | No token matched — inspect **`sys_mmap`** body for that combination of **`prot` / flags / fd**. |

**Reading crash14:** If **`flags`** decodes to normal musl anonymous mmap (**`PRIVATE|ANON`**, no **`FIXED`**) but **`x0`** still looks errno-shaped and **`reason=other`** (or **`EINVAL`** for an unexpected branch), that supports **GPR corruption** rather than the fixed-alignment path alone. If **`FIXED`** appears in the decode and **`reason=fixed+unaligned`**, the kernel is behaving consistently with **unaligned fixed `addr`** even though user source passed **`NULL`** — then prioritize **trap-frame / wrong-register** auditing.

**Trap ELR source:** [`current_trap_frame_elr()`](../crates/akuma-exec/src/threading/mod.rs) reads the pointer saved by **`set_current_trap_frame`** in the EL0 sync handler ([`src/exceptions.rs`](../src/exceptions.rs)).

**Pipe `read` (Pattern 2 parent):** unrelated tag **`[pipe-read]`** — gated by **`SYSCALL_DEBUG_PIPE_READ`** in [`src/config.rs`](../src/config.rs); see [§ Interim conclusions](#interim-conclusions-2026-05-08) for how **`buf=`** vs Go **`fault`** compares.

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

   - **Immediately before** that, kernel prints **`[EINVAL] nr=222 pid=<child>`** with **`args=[0xffffffffffffffea, …]`**. **`nr::MMAP` is 222** in [`src/syscall/mod.rs`](../src/syscall/mod.rs). **`0xffffffffffffffea`** is **`−22` (`EINVAL`)** in **`x0`** — the **first argument to `mmap`** (address hint). The C source only ever passes **`NULL`**; this implies **GPR corruption at syscall entry** (or a logging/TID mix-up—either way, investigate trap path). With **extended errno logging** enabled, compare **`tid`** / **`ELR`** on that line to the faulting thread; read **`[mmap-einval] reason=`** and the **`flags=(…)`** decode—see [Serial errno diagnostics](#serial-errno-diagnostics-2026-05-08).

   - **After** the parent dies, **`mmap_stress`** children often **`exit_group code=0`** — they can **outlive** the parent until **`SIGTERM`** / duration; **child `code=0` does not prove** the parent **`read`** path is safe.

**Interpretation:** Parallel **mmap storm + parent pipe I/O** correlates with **errno-shaped words appearing in syscall argument registers** — seen on **`crash13`** (**xattr / `clock_gettime`**) and now on **`mmap`** in **`crash14`**. That strengthens the hypothesis that **kernel-side syscall / trap-frame / `clone` / `rt_sigreturn` plumbing** must be audited—not only Go’s **`mallocgc`**.

**Regression coverage:** In-kernel tests **`mmap_fixed_addr_unaligned_einval_helper`** and **`mmap_einval_through_handle_syscall`** ([`src/tests.rs`](../src/tests.rs)) pin the **`MAP_FIXED` + unaligned `addr` → EINVAL** path and the **`len==0`** path so **`mmap_fixed_addr_unaligned_einval`** in [`src/syscall/mem.rs`](../src/syscall/mem.rs) cannot drift from **`sys_mmap`** without CI catching it.

**Mitigations while debugging:** `GODEBUG=asyncpreemptoff=1`, ample RAM, smaller `-mmap_alloc_mb` for bisection; reduce contention with **`--num_children=1`** when isolating parent vs child behavior.

## Agent handoff (2026-05-08)

Use this section to pick up **`forktest_parent`** + **`--use_c_child`** + **`-mmap_test`** investigation without re-reading the whole thread. **Root cause is not closed.**

### Scope

- **Parent:** Go binary **`forktest_parent`** ([`userspace/forktest/parent/main.go`](../userspace/forktest/parent/main.go)) — **epoll loop**, **`read`** on pipe FDs, **`EpollCtl`** `EPOLL_CTL_MOD` re-arm ([`main.go`](../userspace/forktest/parent/main.go) ~238–245).
- **Children:** **`/bin/mmap_stress`** (pure C, musl) — heavy **`mmap` / memset / munmap** ([`userspace/forktest/c_stress/`](../userspace/forktest/c_stress/)).
- **Failure:** intermittent **Pattern 2**-style death: **`SIGSEGV`**, userspace reports **`PC≈0x13060`**, fault addresses in **`~0x13……`** / **`~0x129……`** range (not obviously the pipe **`read`** buffer pointer **`~0x1e008……`**).

### Established conclusions (do not re-litigate without new evidence)

1. **`PC=0x13060` is not specific to `read`.** In static **`forktest_parent`**, it is the shared **AArch64 syscall trampoline** (`internal/runtime/syscall/asm_linux_arm64.s` → **`Syscall6`**). Crashes have been attributed in Go stacks to **`unix.Read`**, **`unix.EpollCtl`**, etc.—**the same `PC`** appears for **different syscalls**. **Inferring “pipe read bug only” from `PC` is wrong.**

2. **`crash17.log` (serial + same-session SSH)** — two **`forktest_parent` runs in one boot log:
   - **Run A (`pid=90`, ~`T22`):** Last **`[pipe-read] enter`** with **`fd=4`**, **`buf=0x1e0087708`**, **`cnt=1024`**; **immediately after**, **`tkill(tid=8, sig=23)`** (`SIGURG`); then **`deliver sig=11`**, **`fault_pc=0x13060`**. No **`[pipe-read] EFAULT`** (validate or **`copy_to_user`**) before death.
   - **Run B (`pid=97`, ~`T92`):** Dense **`[pipe-read] enter`** storm; **no** adjacent **`tkill(sig=23)`** line before **`sig=11`** in the captured window; **same** **`fault_pc=0x13060`**, **`slot=8`**. SSH traceback for run B showed **`EpollCtl`** at **`main.go:243`** (re-arm), not **`Read`**—consistent with **§1**.

3. **Fault address vs buffer.** Go **`fault` / `addr`** (e.g. **`0x1343f000`**, **`0x13a53000`**) has **not** been shown to lie in **`[buf, buf+count)`** for the **`read`** shown in registers (**`buf≈0x1e0087708`**, **`count=0x400`**). Do **not** assume a trivial **`copy_to_user`**-to-pipe-buffer failure without kernel **`FAR`** / **`copy_to_user`** path proof.

4. **`GODEBUG=asyncpreemptoff=1`** — In at least one session, **first** forktest run **succeeded**, **second** run in the **same shell** still **crashed**. **`asyncpreemptoff`** is **not** a reliable fix. Treat **`SIGURG`** as **sometimes correlated** (see run A), **not** always present (run B) and **not** proven sole cause.

5. **Children / `mmap`:** Serial **`[EINVAL] nr=222`** with **`x1=0`**, errno-shaped or mmap-base-shaped **`x0`**, garbage **`prot`/`flags`** → **`reason=len==0`** is real; **decoded `flags=(FIXED|…)` text may be meaningless** when registers are garbage. Supports **SVC entry / GPR corruption** under load, **not** “musl called `mmap` wrong” in source.

### Already in tree (landed before next agent)

| Item | Location |
|------|----------|
| Extended errno lines (`tid`, `ELR`, full args; **`[mmap-einval]`**) | [`src/syscall/mod.rs`](../src/syscall/mod.rs), **`SYSCALL_ERRNO_DIAG_EXTRA`** in [`src/config.rs`](../src/config.rs) |
| **`[pipe-read]`** tracing | [`src/syscall/fs.rs`](../src/syscall/fs.rs), **`SYSCALL_DEBUG_PIPE_READ`**, **`SYSCALL_DEBUG_PIPE_READ_SAMPLE`** in [`src/config.rs`](../src/config.rs) |
| **`mmap`** unaligned fixed **`EINVAL`** before **`lookup_process`** | [`src/syscall/mem.rs`](../src/syscall/mem.rs) |
| Regression tests | [`src/tests.rs`](../src/tests.rs): **`mmap_fixed_addr_unaligned_einval_helper`**, **`mmap_einval_through_handle_syscall`** |
| **`current_trap_frame_elr()`** | [`crates/akuma-exec/src/threading/mod.rs`](../crates/akuma-exec/src/threading/mod.rs) |
| Log grep helper | [`scripts/analyze_kernel_log.sh`](../scripts/analyze_kernel_log.sh) |

### Open hypotheses (priority order)

1. **Trap frame / syscall return / `rt_sigreturn`** loses or swaps GPRs across **nested signals** (`SIGURG`, **`SIGSEGV`**) or across **scheduler / syscall** boundaries.
2. **`copy_from_user`** / **`copy_to_user`** interaction with **lazy mappings** or **wrong ASID / TTBR / owner PID** for **`CLONE_VM`**-adjacent bugs — less ruled out than once thought after **`tgid`** fixes, still worth verifying on **parent** path.
3. **Epoll + pipe** interaction bugs — **secondary** until **`x8`** (syscall nr) at fault is proven; **`read`** and **`epoll_ctl`** share **`PC`**.

### Recommended next steps (for the next agent)

1. **Log syscall number at fault:** On **`SIGSEGV`** delivery or EL0 fault when **`ELR≈0x13060`**, print **`x8`** and a short name (**`63`**=`read`, **`21`**=`epoll_ctl` on Linux aarch64—verify constants in-tree). Removes ambiguity between **`read`** vs **`EpollCtl`**.
2. **If `x8=21`:** Audit **[`sys_epoll_ctl`](../src/syscall/poll.rs)** (`copy_from_user` for **`epoll_event`**, **`validate_user_ptr`**).
3. **If `x8=63`:** Trace **[`sys_read`](../src/syscall/fs.rs)** **`PipeRead`** path end-to-end; confirm whether **`copy_to_user`** can return **`EFAULT`** without logging (already logs when **`SYSCALL_DEBUG_PIPE_READ`** on).
4. **A/B:** **`GODEBUG=asyncpreemptoff=1`** vs off — tabulate **`tkill(sig=23)`** vs crash.
5. **Load bisection:** **`--num_children=1`**, **`-mmap_alloc_mb=10`** vs **70**.
6. **Control experiment:** minimal **C parent** (pipes + **`epoll_pwait`** + **`read`** + **`epoll_ctl`**) + same **`mmap_stress`** children — if stable, shift focus to **Go runtime**; if not, **kernel**.

### Key kernel files

[`src/exceptions.rs`](../src/exceptions.rs) (`rust_sync_el0_handler`, **`try_deliver_signal`**, **`do_rt_sigreturn`**) · [`src/syscall/fs.rs`](../src/syscall/fs.rs) · [`src/syscall/poll.rs`](../src/syscall/poll.rs) · [`src/syscall/pipe.rs`](../src/syscall/pipe.rs) · [`src/syscall/mod.rs`](../src/syscall/mod.rs)

### Reference captures

**`crash16.log`**, **`crash17.log`** (repo or local paths as provided by the user), **`crash14`** / **`crash5`** discussed above—grep patterns in [Serial capture and grep](#serial-capture-and-grep) and **`analyze_kernel_log.sh`**.
