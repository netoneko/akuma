# Fix Memory Mapping: Correctness, Performance, and Missing Flags

## Background

Kernel freezes kept recurring during `go build` despite 15+ fixes since 2026-03-21. Root-cause
analysis of `docs/GOLANG_IPC.md` identified three classes of remaining problems:

1. **Correctness bugs** — OOM in fault handlers, timer loss, fault_mutex leaks causing permanent deadlock
2. **Performance bottlenecks** — per-page TLB flushes, no batch allocation in IA handler, eager full fork copy
3. **Missing syscall flags** — CLONE_SETTLS, CLONE_CHILD_CLEARTID, MAP_POPULATE silently ignored

See also `proposals/FASTER_MEMORY_MAPPING.md` for the original CoW/huge-page/background-zeroing proposal.

---

## Phase 1: Correctness Fixes

### 1A: OOM fallback in demand-paging readahead ✅

**Problem:** In both the Data Abort (`EC_DATA_ABORT_LOWER`) and Instruction Abort
(`EC_INST_ABORT_LOWER`) file-backed readahead paths, if `frame_pool` runs out before the
*faulting* page itself (`page_va`) is mapped, `any_mapped` stays false and the handler falls
through to SIGSEGV delivery. The fault re-triggers immediately → infinite loop → kernel freeze.

**Fix:** After the readahead loop, if `!any_mapped`, attempt a single `alloc_page_zeroed()` for
just `page_va` before falling through to SIGSEGV.

**Files changed:** `src/exceptions.rs`
- DA file-backed path: after the `if any_mapped { return }` block
- IA file-backed path: same fallback

### 1B: fault_mutex RAII guard ✅

**Problem:** Instruction abort handler inserts `page_va` into `fault_mutex` BTreeSet at line ~2161
and removes it at line ~2178 (before the actual mapping work). Any early return or panic between
those two points leaks the entry permanently, deadlocking that page address forever.

**Fix:** Use a struct `FaultGuard` that removes the entry in `Drop`, ensuring cleanup on all paths.

**Files changed:** `src/exceptions.rs`

### 1C: Timer re-arm hardening ✅

**Problem:** Per `GOLANG_IPC.md` "Kernel Freeze at T222s" and "Kernel Freeze During Go Build":
the `[TMR]` heartbeat stops ~26 seconds before the visible hang. If `cntp_ctl_el0` ever gets
corrupted (enable=0 or mask=1), no further timer IRQs fire, no preemption, system dies.

**Fix:** At the end of `timer_irq_handler`, write `cntp_ctl_el0 = 1` to keep enable=1, mask=0,
even if something corrupted it.

**Files changed:** `src/interrupts.rs` or `src/timer.rs` (wherever `timer_irq_handler` lives)

---

## Phase 2: Batch TLB Invalidation ✅

**Problem:** `map_user_page` does `dsb ishst + tlbi vale1is + dsb ish + isb` per page. For
256-page readahead that is 256 full barrier sequences. `dsb ish` stalls the entire pipeline on
AArch64.

**Fix:**
- Add `map_user_page_no_flush(va, pa, flags)` — skips TLB invalidation
- Add `flush_tlb_range(asid, start_va, pages)` — loops `tlbi vale1is` then single `dsb ish + isb`
- Update DA readahead, IA readahead, `sys_mmap` eager loop, and `sys_mprotect` to use
  `map_user_page_no_flush` inside loops + `flush_tlb_range` after

**Files changed:**
- `crates/akuma-exec/src/mmu/mod.rs` — new functions
- `src/exceptions.rs` — DA and IA readahead loops
- `src/syscall/mem.rs` — `sys_mmap` eager path, `sys_mprotect`

**Expected impact:** ~100x fewer pipeline-stalling barriers for large mappings.

---

## Phase 3+4: Batch Allocations ✅

**Phase 3 — IA handler batch alloc:**
Port the DA handler's `alloc_pages_zeroed(needed)` pattern to the Instruction Abort handler.
Previously IA allocated one page at a time (acquiring PMM spinlock 256 times for readahead).

**Phase 4 — eager mmap batch alloc:**
Replace `sys_mmap`'s per-page `alloc_page_zeroed()` loop with a single `alloc_pages_zeroed(pages)`
call, falling back to per-page only if the batch fails.

**Files changed:** `src/exceptions.rs` (IA handler), `src/syscall/mem.rs` (sys_mmap)

---

## Phase 5: Fork/Clone Benchmark ✅

Added kernel benchmarks to `src/tests.rs`:

- **`bench_fork_clone`** — creates a process with 1 MB brk + 10 mmap regions of 256 pages,
  measures `fork_process` wall time via `uptime_us()`, reports MB/s throughput
- **`bench_mmap_eager`** — times eager `sys_mmap` for 256 pages
- **`bench_demand_page`** — times single-page demand-paging fault handling

Run with the kernel test harness. Use before/after to quantify improvement from Phase 2+3+4.

**Files changed:** `src/tests.rs`

---

## Phase 6: Missing Syscall Flags ✅

### CLONE flags — already implemented

Investigation showed all key CLONE flags were already handled:

- **CLONE_SETTLS (0x80000):** `clone_thread` sets `child_ctx.tpidr = tls` — already correct.
- **CLONE_PARENT_SETTID (0x100000):** `clone_thread` writes child PID to `parent_tid_ptr` — already correct.
- **CLONE_CHILD_SETTID (0x1000000):** `clone_thread` writes child PID to `child_tid_ptr` — already correct.
- **CLONE_CHILD_CLEARTID (0x200000):** `clone_thread` stores pointer in `proc.clear_child_tid`;
  `return_to_kernel` writes 0 and calls `futex_wake` — already correct.
- `sys_set_tid_address` updates `proc.clear_child_tid` — already correct.
- `replace_image` (execve) resets `clear_child_tid = 0` — already correct.

### MAP_POPULATE (0x8000) — added

When set, `sys_mmap` skips the lazy-region path even for large allocations (`pages > 256`).
If the batch PMM allocation fails, gracefully falls back to lazy rather than returning ENOMEM.

### MADV_WILLNEED (advice=3) — added

`sys_madvise` now pre-faults pages in `[addr, addr+len)` that are in lazy regions but not yet
mapped. Uses batch allocation (`alloc_pages_zeroed`) + `map_user_page_no_flush` per page +
single `flush_tlb_range` after. OOM is silently ignored (advisory syscall).

### MAP_SHARED warning — added

`sys_mmap` logs a warning when `MAP_SHARED` is used on file-backed mappings (unsupported; treated
as `MAP_PRIVATE`). `MAP_SHARED | MAP_ANONYMOUS` is safe — equivalent to `MAP_PRIVATE`.

### MAP_STACK (0x20000) — noted

Hint-only on Linux; safe to ignore. Added as a comment in the constant definitions.

**Files changed:** `src/syscall/mem.rs`

---

## Phase 7: Lazy Region O(log n) Lookup ✅

**Problem:** `lazy_region_lookup_for_pid` does BTreeMap lookup by PID, then linear scan of
`Vec<LazyRegion>`. Go processes create 100+ lazy regions (one per heap arena), making each page
fault O(n) in region count.

**Fix:** Change per-PID storage from `Vec<LazyRegion>` to `BTreeMap<usize /*start_va*/, LazyRegion>`.
Use `range(..=va).next_back()` for O(log n) lookup: find the region with the largest `start_va ≤ va`,
then verify `va < start_va + size`.

**Files changed:**
- `crates/akuma-exec/src/process/children.rs` — all LAZY_REGION_TABLE functions
- `crates/akuma-exec/src/process/table.rs` — type definition update

---

## Phase 8: Fork Lazy Copy Performance + Signal Frame Fixes

### 8A: Fork lazy copy hang ✅

**Problem:** Fork's lazy page copy iterates every page in every lazy region calling
`translate_user_va` individually. A mature Go process has 30+ lazy regions totaling hundreds of
MB of virtual space (Go heap arenas are 64MB each). On QEMU TCG each translate costs ~2–10µs,
so iterating 50,000–250,000+ pages causes a multi-second hang with no progress output.
The log ends at `[FORK-DBG] step4: mmap done` and never reaches `step4: lazy done`.

**Fix:**
1. Keep `FORK_IN_PROGRESS=true` during lazy copy (was set to false before the slow section)
2. Added per-region progress logging (every 4 regions)
3. Replaced per-page `translate_user_va` with `collect_mapped_pages_sparse()` — walks page
   tables at L2 granularity (2MB = 512 pages), skipping empty L0/L1/L2 entries entirely.
   For a 256MB sparse region with 1% density: 65,536 → ~128 L2 checks + ~650 L3 lookups.
4. Added timing to lazy copy section.

**Files changed:**
- `crates/akuma-exec/src/process/mod.rs` — fork lazy copy rewrite
- `crates/akuma-exec/src/mmu/mod.rs` — new `collect_mapped_pages_sparse()` function

### 8B: Signal frame uc_stack ✅

**Problem:** Signal frame construction zeroes the entire frame but never fills
`ucontext.uc_stack` (ss_sp, ss_flags, ss_size) or `uc_sigmask`. Go's runtime reads uc_stack
to determine if the signal arrived on the sigaltstack. All-zero uc_stack confuses Go's panic
recovery, causing corrupted SP (0x80000000) and PSTATE (all DAIF masked) on sigreturn.

**Fix:** After zeroing the frame, write sigaltstack info into uc_stack fields:
- `ss_sp` at ucontext+16 (alt_sp)
- `ss_flags` at ucontext+24 (SS_ONSTACK=1 if on altstack, else 0)
- `ss_size` at ucontext+32 (alt_size)
- `uc_sigmask` at ucontext+40 (proc.signal_mask before blocking)

**Files changed:** `src/exceptions.rs` — signal frame construction in `try_deliver_signal`

### 8C: IC flush + signal delivery ✅

**Problem:** JIT IC flush replay path (bogus syscall nr > 500) returns early from the SVC
handler, bypassing the pending signal delivery check. SIGURG preemption is delayed until the
next normal syscall, adding up to 10ms latency to Go's goroutine preemption.

**Fix:** Added pending signal check after IC flush + ELR backup, before returning. If a
signal is pending (e.g. SIGURG), deliver it immediately via `try_deliver_signal`.

**IMPORTANT constraint:** Only async/preemption signals may be delivered here. Fault signals
(SIGSEGV=11, SIGBUS=7, SIGFPE=8, SIGILL=4) carry specific `si_addr` from the original fault.
Delivering them with the IC flush's fault_pc causes Go's `sigpanic` handler to try patching
code at the wrong address, which itself faults → re-entrant SIGSEGV → process killed.
Implementation uses `effective_mask = sig_mask | FAULT_SIGNALS` when calling `take_pending_signal`.

**Files changed:** `src/exceptions.rs` — IC flush path in `sync_el0_handler`

---

## Phase 9: AIO Stubs + MAP_SHARED Read-Only ✅

### 9A: io_getevents / io_submit / io_cancel stubs ✅

**Problem:** Go compile worker (PID 143) crashed because syscall 4 (io_getevents) returned
ENOSYS (-38). Go treated the return value as a pointer, causing WILD-DA at
FAR=0xffffffffffffffda. The crash killed PID 143; Go build saw the failure and exited code=1
without ever invoking the linker. All other ~60 compile workers succeeded.

Syscalls 0 (io_setup) and 1 (io_destroy) were already implemented with a proper AIO ring
buffer, but syscalls 2/3/4 returned ENOSYS — inconsistent with a working io_setup.

**Fix:** Added stub implementations:
- `sys_io_submit`: validates ctx, returns 0 (no events submitted)
- `sys_io_cancel`: returns EINVAL (no outstanding requests)
- `sys_io_getevents`: returns 0 (no events ready, consistent with empty ring head==tail)

All stubs return proper error codes (EINVAL) for invalid contexts, never ENOSYS.

**Files changed:** `src/syscall/aio.rs`, `src/syscall/mod.rs`

### 9B: MAP_SHARED file-backed read-only ✅

**Problem:** 76 `MAP_SHARED file-backed unsupported` warnings during go build. Go's build
system mmaps compiled object files with MAP_SHARED for reading. While MAP_PRIVATE fallback
is functionally correct for read-only access, the warnings are noisy and misleading.

**Fix:** Suppress warning for read-only MAP_SHARED file-backed mappings (PROT_WRITE not set).
Only warn for writable MAP_SHARED, which would require true shared-page semantics not yet
implemented.

**Files changed:** `src/syscall/mem.rs`

---

## Phase 10: IC Flush Regressions ✅

Two regressions introduced by Phase 8C's IC flush signal delivery, both triggered by
Go's compile workers during heavy JIT compilation.

### 10A: IC flush delivers SIGSEGV with wrong context ✅

**Problem:** `take_pending_signal` in the IC flush path could return SIGSEGV (sig=11). The
signal was delivered with the IC flush's `fault_pc` (the SVC instruction address), not the
original fault address. Go's `sigpanic` handler received SIGSEGV, looked at `fault_pc` ≈ JIT
code, and tried to patch the instruction at that address (a code page → read-only). Write
fault at the code address → second SIGSEGV → re-entrant SIGSEGV → process killed.

Crash signature: `[signal] sig 11 re-entrant FAULT at 0x1002a11c` with ISS=0x4f (L3
permission fault, write attempt). Killed PID 55 (/usr/lib/go/bin/go) after 0.25s.

**Fix:** Block fault signals in the IC flush signal delivery path:
```
const FAULT_SIGNALS: u64 = (1 << 4) | (1 << 7) | (1 << 8) | (1 << 11); // SIGILL,SIGBUS,SIGFPE,SIGSEGV
let effective_mask = sig_mask | FAULT_SIGNALS;
```
Fault signals are left pending and delivered at the next normal signal delivery point
(after syscall return, after exception handler) where `si_addr` is correct.

**Files changed:** `src/exceptions.rs` — IC flush path in `sync_el0_handler`

### 10B: IC flush replays SVC with wrong register state → spurious io_setup ✅

**Problem:** IC flush backs up `ELR -= 4` to replay the instruction before the bogus SVC.
If that instruction is also a SVC (a different syscall), it fires with the IC flush
trampoline's register state instead of the registers Go prepared for that syscall.

Observed: IC flush at ELR=0x1009e478, ELR-4=0x1009e474 contained an `io_setup` SVC (x8=0).
io_setup was called with `x0=0x20175d008, x1=0x1` (IC flush trampoline values) instead of
the intended args. `validate_user_ptr(ctx_idp=0x1, 8)` failed → EFAULT. Go stored the EFAULT
return value, later dereferenced it as a pointer → WILD-DA at FAR=0xfffffffffffffff2.

**Fix:** Before setting `ELR -= 4`, peek at the instruction at ELR-4 using
`copy_from_user_safe`. If it matches the AArch64 SVC encoding
`(instr & 0xFFE0001F) == 0xD4000001`, skip the ELR backup. The IC flush clears QEMU's TB;
on resume, QEMU retranslates from the unchanged ELR and executes the new code correctly.

Log output distinguishes the two cases:
```
[JIT] IC flush + replay #1 bogus nr=... ELR=0x... prev=replay   ← normal, ELR backed up
[JIT] IC flush + replay #1 bogus nr=... ELR=0x... prev=SVC(skip) ← new case, ELR unchanged
```

**Files changed:** `src/exceptions.rs` — IC flush path in `sync_el0_handler`

---

## Phase 11: Copy-on-Write Fork (future)

**Not yet implemented.** Highest impact (eliminates the multi-second fork copy for Go's 50+ MB
heap) but most complex. Requires:
1. PMM frame reference counting
2. CoW page table duplication in `fork_process`
3. Write permission fault handler in DA path
4. Feature gate behind `config::COW_FORK_ENABLED`

Deferred until Phases 1–10 have stabilized the system under `go build`.

---

## Verification

After each phase:
```
cargo check
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Boot QEMU and run:
```
go version                                          # smoke test
CGO_ENABLED=0 go build -x -v -o ./hello_go .       # stress test
```

Check serial log: `[TMR]` heartbeat must never stop; no demand-paging SIGSEGV.
