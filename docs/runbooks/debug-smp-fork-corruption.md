# SMP=4 fork/exec process-state corruption — handoff / debugging dossier

> **Status: FIX LANDED 2026-07-21 — awaiting empirical confirmation.** The
> "correctness-first bisection" experiment (suggested next experiments §3) is
> implemented: a reentrant `LifecycleLock` now serializes every process-lifecycle
> op (`fork_process` / `vfork_process` / `clone_thread` / `replace_image{,_from_path}`
> / `return_to_kernel{,_from_fault}` / `kill_process{,_with_signal}` /
> `spawn_process_with_channel_ext` / `spawn_process_from_image_with_args`) across
> preemption under `cfg(kernel_smp_shared)`. Source:
> `crates/akuma-exec/src/process/lifecycle.rs`. The lock is **distinct from the
> BKL**, held with IRQs **enabled** (so the holder can still be preempted and
> resumed — preemption no longer exposes half-built state because no peer can
> enter a lifecycle op until the holder finishes), and reentrant (depth-tracked)
> so nested calls like `return_to_kernel → kill_box → kill_process` don't
> self-deadlock. Compiles to a no-op on every non-`kernel_smp_shared` profile
> (default / `size` / `extreme` / `multikernel`), so those builds are byte-for-byte
> unaffected. **To confirm:** rerun `SMP=4 python3 sshd_crash_hunt.py` against a
> fresh `cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests`.
> If the crash vanishes → hypotheses 1/3 confirmed and the long-term fix is real
> per-Process locking (RwSpinlock) for non-lifecycle readers. If it persists →
> narrowed to hypotheses 2/4 (THREAD_CONTEXTS aliasing or TLB coherence), and
> this lock can stay as defense-in-depth regardless. Below is the original handoff
> doc, unchanged for context.
>
> Companion: [`debug-smp.md`](debug-smp.md) (general shared-kernel SMP debugging)
> and [`../reference/subsystems/smp-shared.md`](../reference/subsystems/smp-shared.md).

## One-paragraph summary

Under **`cfg(kernel_smp_shared)` at SMP=4**, during the **high-concurrency bringup window**
(secondaries onlining while `herd` fork+execs every service, plus a fork-hammer of
`busybox` over ssh), processes **SIGSEGV with heterogeneous signatures** — the hallmark of
**memory corruption of `Process` / saved-context / page-table state**, not a single logic
bug. It hits **both freshly-forked children *and* already-running processes** (e.g. `/bin/sshd`
faults at 2.89 s uptime). The kernel itself stays alive (0 `[BKL] stuck`, heartbeats
continue) — this is a userspace-visible fault caused by corrupted per-process state. A
**settled** instance is stable (survives 30×20 concurrent fork rounds); the corruption is
specific to the concurrent bringup window.

## Exact repro

```bash
# Build the SMP devbox image (no-tests, userspace sshd, smoltcp, real SMP):
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests

# Auto-repro harness: reboots at SMP=4, waits for sshd, fork-hammers it, greps for the fault.
# (Harness lives in scratchpad; reproduced verbatim at the bottom of this doc.)
SMP=4 python3 sshd_crash_hunt.py
# Writes sshd_crash_HUNT_RESULT.txt / _PROGRESS.txt / sshd_hunt_boot.log in the repo root.
```

It reproduces **within one boot** most of the time (caught on boot 1/20 in the last run).
The fork-hammer is: 16 concurrent ssh connections, each running
`for i in 1 2 3 4 5 6 7 8; do busybox true; done` — i.e. a burst of `fork`+`execve("/bin/busybox")`.

## The signatures (heterogeneous ⇒ corruption, not one logic bug)

All observed in a **single** boot:

- **Null / near-null deref in userspace.** `FAR=0x0` or `FAR=0x120` (a struct-field offset off
  a null base), `ELR` a *valid* busybox code address, `x0=0`. musl/busybox dereferences a
  pointer that should be valid but reads as 0.
- **User PC = a *kernel* address.** `[WILD-DA] pid=22 FAR=0x0 ELR=0x4011d004` with `SPSR=0x0`
  (EL0t). The kernel is based at `0x40100000`, so `0x4011d004` is *kernel text* running as the
  thread's EL0 PC — the saved user context's `pc` was clobbered with a kernel value.
- **Clobbered / half-built `Process`.** `[DA-MISS] pid=23 ppid=0 … checked 0 mmap_regions`,
  `[DA-MISS] pid=8 ppid=0 va=0x120 parent_lr=0`. A process whose `Process.parent_pid == 0`
  (no real parent) and/or empty `mmap_regions` — fields that fork/exec set to non-zero for a
  real process. **NB pid 8 is an already-running service, not a fresh child** — its `parent_pid`
  was zeroed *after* it was healthy.

Raw dump (drops-OFF run, boot 1):

```
[T2.16] [Fault] Data abort from EL0 at FAR=0x0, ELR=0x100b5ff4, ISS=0x47
[Fault]  x0=0x0 x1=0x10120460 x2=0x10124220 x3=0x13
[Fault]  x19=0x20124620 x20=0x10120000 x29=0x202ffff2f0 x30=0x100b6254
[Fault] Process 21 (/bin/busybox) SIGSEGV after 0.06s
[DA-MISS] pid=22 ppid=5 va=0x0 lr_count=6 parent_lr=6 parent_has_va=false
...
[T2.17] [WILD-DA] pid=22 FAR=0x0 ELR=0x4011d004 last_sc=...   # user PC = KERNEL addr
[Fault] Process 22 (/bin/busybox) SIGSEGV after 0.06s
[DA-MISS] pid=23 ppid=0 va=0x0 lr_count=5 parent_lr=0 parent_has_va=false   # ppid=0
[T2.17] [DP] eager miss: pid=23 va=0x0 checked 0 mmap_regions               # empty mmap_regions
```

## THE decisive experiment (already run — do not repeat)

**Hypothesis tested:** the two BKL-drop optimizations (execve ELF read + file-fault block I/O)
open a window where concurrent EL1 corrupts shared state.

**Method:** forced **both** drops OFF at boot (see the temporary edit in `src/main.rs` right
after the `no-tests` `bringup_secondaries()` call — marked *DO NOT COMMIT*):

```rust
smp_shared::set_exec_bkl_drop_enabled(false);
smp_shared::set_fault_bkl_drop_enabled(false);
```

**Result: the crash STILL fires on boot 1/20, all three signatures present.**

**Why this is decisive.** `rust_sync_el0_handler` (`src/exceptions.rs:2116`) wraps the *entire*
syscall+fault path in `bkl::enter_kernel()` / `bkl::leave_kernel()` (lines 2117 / 2123). The
**only** BKL releases *inside* an excursion are the two drops we just disabled. So with the
drops off, **every EL1 excursion holds the BKL end-to-end → no two cores ever execute EL1 at
the same instant.** The corruption persists anyway ⇒ **it is NOT caused by concurrent EL1
execution and NOT by the BKL-drop windows.** (This overturns the earlier working theory in
`debug-smp.md` that said "audit the BKL-drop sites.")

## Ruled OUT (with evidence — don't re-chase)

1. **BKL-drop windows** (execve ELF read `src/syscall/proc.rs:645`; file-fault block I/O
   `src/exceptions.rs:2774,3303`). Disproven by the decisive experiment above. Also: each drop
   is scoped to touch only *private / not-yet-installed* frames — no live `&mut Process` or
   process-table mutation crosses the drop.
2. **CoW share / break / eviction / refcount.** Cross-core *defended*: `pmm::free_page`
   (`src/pmm.rs:569`) is refcount-aware via `cow_ref_dec` (only frees at count 0); the CoW-break
   fault handler copies the source frame *before* decrementing (`src/exceptions.rs:2584-2600`);
   the PTE edit is under the per-AS `as_lock`. `try_evict_ro_page` → `free_page`
   (`crates/akuma-exec/src/mmu/mod.rs:823`, `.../process/children.rs:537`) is safe for the same
   reason — it can drop this AS's ref on a shared frame without freeing it under a peer.
3. **Fork never drops the BKL / never yields.** `handle_oom` (`src/allocator.rs:52`) grows the
   heap synchronously or returns `Err`; no allocation inside `fork_process` yields. A single
   `fork_process` is therefore *instantaneously* atomic w.r.t. EL1 (but see the preemption
   caveat below — it is **not** atomic across preemption).
4. **Thread-slot-reuse context zeroing** (cleanup zeroing a slot spawn just filled). Guarded by a
   `TERMINATED → INITIALIZING` SeqCst CAS (`crates/akuma-exec/src/threading/mod.rs:910-950`).
5. **ext2 / block read path.** Properly SMP-locked: `state: RwSpinlock<Ext2State>` +
   `block_cache: Spinlock<BlockCache>` + `BLOCK_DEVICE: Spinlock` (`crates/akuma-ext2/src/ext2.rs:529-547`,
   `src/block.rs:226`). Concurrent reads serialize correctly.

## The structural hole (where the bug almost certainly lives)

The process/threading subsystem's cross-core safety rests on **single-CPU invariants upgraded to
"the BKL serializes EL1"**, *not* on locks protecting the data:

- **`THREAD_CONTEXTS`** — the per-thread saved register file (`UnsafeCell`, no lock). Its safety
  comment literally reads *"3. We're single-CPU, so no concurrent access is possible"*
  (`crates/akuma-exec/src/threading/mod.rs:1377-1385`). Accessed with only `with_irqs_disabled`.
- **The process table** (`crates/akuma-exec/src/process/table.rs`) hands out
  `&'static mut Process` to 218+ sites via `current_process()` / `lookup_process()` /
  `get_process_ptr()`, guarded only by `with_irqs_disabled` (`table.rs:112-143`). The safety
  comment says valid "while IRQs are disabled **or** no other thread can call
  `unregister_process`" — a single-core statement. `with_irqs_disabled` takes **no cross-core
  lock**; it only masks local IRQs.

**The critical realization about the BKL's guarantee.** IRQs are **enabled** during the
syscall/fault handler (`src/exceptions.rs:174`, `msr daifclr, #2`). So a thread can be
**preempted mid-`fork`/`execve`/`exit`**. On that preemption the IRQ path reconciles the BKL to
the frame it `eret`s into — and if it switches to an **EL0** thread it **releases the BKL**
(`src/exceptions.rs:1512,1543`; the eret in `rust_sync_el0_handler` releases at line 2123 *before*
the asm restores registers). Therefore:

> The BKL guarantees no two cores run EL1 **at the same instant**. It does **NOT** make a
> multi-step kernel operation (`fork_process`, `do_execve`/`replace_image`, exit/teardown)
> **atomic across preemption**. A half-mutated global (`THREAD_CONTEXTS[tid]`, a `Process`
> mid-construction, a process-table slot mid-registration, a `Process` mid-`replace_image` with
> `mmap_regions` already `.clear()`ed) is exposed at every preemption point to whatever EL1 code
> the next-scheduled thread runs — including on another core.

And separately, **EL0 runs with no BKL at all** (genuine parallelism): two `busybox` children
execute userspace simultaneously on different cores over frames the fork **CoW-shared** between
parent and child (`crates/akuma-exec/src/process/mod.rs:1555-1726`). Correctness there depends on
the demote-to-RO + `flush_tlb_all()` (`mod.rs:1717-1726`) being coherent before either side runs,
and on the CoW-break protocol being atomic across the two *separate* per-AS `as_lock`s.

## Narrowed hypothesis space (rank-ordered for the next debugger)

1. **`replace_image` (execve) is not atomic across preemption.** The tell is strong: crashes
   cluster right after `[FORK-DBG] replace_image: … AS swapped` / `trampoline ENTRY`, and
   `replace_image` **`.clear()`s `mmap_regions`/`lazy_regions`** mid-flight
   (`crates/akuma-exec/src/process/image.rs:49,124`) and swaps the address space
   (`deactivating old AS` → `swapping AS`). If the exec'ing thread is preempted between the
   clear/AS-swap and repopulation, and *anything* reads that `Process` (a signal, a
   `for_each_process` sweep, its own re-entry, a sibling), it sees a half-built image →
   `checked 0 mmap_regions`, `ppid`/context garbage. **Start here.**
2. **`THREAD_CONTEXTS[tid]` clobbered → user PC = kernel addr. ⭐ Strongest concrete lead.**
   `0x4011d004` **resolves exactly to `akuma::exceptions::rust_sync_el0_handler_inner + 0x0`**
   (via `llvm-nm -nC` on `target/aarch64-unknown-none/release-smp-shared/akuma`) — the entry of
   the syscall/fault handler. Crucially, **that function's address is never taken as a value in
   the source** — it is only `bl`'d (`src/exceptions.rs:178`) / called
   (`src/exceptions.rs:2122`). So the value in the saved-context `pc` slot is **not** a legitimate
   stored function pointer; it arrived by **memory corruption / aliasing of the context memory**
   itself (a `THREAD_CONTEXTS[tid]` entry or the on-kernel-stack `UserTrapFrame` being reused,
   freed-and-reallocated, or overlapped by another live structure and then read back as a
   `UserContext`). Trace every writer of `THREAD_CONTEXTS` (`update_thread_context`
   `threading/mod.rs:2556`; the SGI context-save; `get_saved_user_context` `:3349`; the fork
   capture `process/mod.rs:1963-2013`) and every place a `UserContext`/`UserTrapFrame`'s backing
   memory could be aliased or reused across a tid-index collision or a preemption. The exact
   `+0x0` value argues against a stack-return-address leak (those land mid-function) and for a
   whole-struct overwrite / index-collision. **`0x100d9ea0` / `0x100b5ff4` etc. are the *user*
   PCs — busybox text; only the `0x401xxxxx` values are kernel.**
3. **`&'static mut Process` aliasing across preemption.** `fork_process` holds
   `let parent = current_process()` (`process/mod.rs:1376`) across its whole body with IRQs
   enabled. If the parent (or a co-owner of that `Process`) runs EL1 after a preemption while
   this `&mut` is live, that is aliasing UB + a data race. Audit long-lived `&'static mut Process`
   held across any point where IRQs are on.
4. **TLB / instruction-cache coherence of the fork CoW demotion + child first-run.** Lower
   probability (drops-off keeps EL1 serialized and `flush_tlb_all` is a broadcast
   `tlbi vmalle1is`), but a stale RW TLB entry on the core that runs the child first — before the
   demotion flush is observed — would let parent and child both write a shared frame with no
   fault. Check the ordering between `flush_tlb_all()` (`mod.rs:1726`) and the child becoming
   schedulable, and whether the child's first `eret` (`entry_point_trampoline` →
   `enter_user_mode`) guarantees an ISB/TLB-clean on the running core.

## Suggested next experiments

- ~~Resolve `0x4011d004`~~ **DONE:** it is `rust_sync_el0_handler_inner+0x0` (see hypothesis 2) —
  a corrupted/aliased context, not a legitimate pointer store.
- **SMP=1 control with the same harness** — expected clean (confirms it's a true cross-core /
  preemption race, not a fork/exec logic bug). The docs assert SMP=1 stability; confirm with
  *this* harness to remove all doubt.
- **Make `fork`/`execve`/exit atomic across preemption**, as a correctness-first bisection: mask
  IRQs (or hold a dedicated *process-lifecycle* spinlock that is NOT dropped on context switch —
  distinct from the BKL) for the whole `fork_process` / `do_execve`+`replace_image` /
  teardown critical section, so no preemption can expose their half-built state. If the crash
  vanishes, the bug is a not-atomic-across-preemption lifecycle op (hypotheses 1/3) and the
  proper fix is real locking on the process table / `Process` fields / `THREAD_CONTEXTS`. If it
  persists, it's the parallel-EL0 / TLB path (hypothesis 4). **Note:** IRQs-off across the ELF
  *read* would re-introduce the block-I/O stall the drops were added to avoid — mask only around
  the state-mutation, do block I/O into private buffers first (the split already exists in the
  fault path; mirror it).
- **Live lldb over the gdbstub** (`INSTANCE=1 GDB=1 SMP=4 …`, attach on `:1235`; see
  `debug-smp.md`). A watchpoint on the victim `Process.parent_pid` / its `THREAD_CONTEXTS[tid].pc`
  catches the corrupting write in the act. Caveat (from the memory notes): the stub's periodic
  all-core halts perturb tight races — the coarse fork-hammer here should still trip.

## Environment / build facts the next debugger needs

- Feature/profile: `--profile release-smp-shared --features devbox-smoltcp,no-tests`.
- `devbox.img` (1 GiB ext2) + `MEMORY=4096` default; QEMU `-smp 4` via `SMP=4`.
- BKL model: owner-tracked, idempotent, **held iff a core is in EL1**, reconciled at EL
  transitions; contended acquire is a **fair FIFO ticket** wait
  (`crates/akuma-exec/src/sync.rs`, driven via `akuma_exec::bkl`).
- The temporary drops-off experiment edit is in `src/main.rs` (marked **DO NOT COMMIT**); revert
  it before shipping. It does not need to stay for debugging — the bug reproduces with drops on
  or off.
- All SMP-shared code is `cfg(kernel_smp_shared)`-gated; default/size/extreme/multikernel builds
  compile none of it.

## Appendix: repro harness (`sshd_crash_hunt.py`)

Reboots devbox-smoltcp at SMP=4 up to 20 times; per boot: wait for `Started sshd` (or a
boot-time crash), then 10 rounds × 16 concurrent ssh connections each running a `busybox true`
fork loop; grep the boot log for `SIGSEGV` / `abort from EL0`; on a hit, dump the surrounding
fault lines to `sshd_crash_HUNT_RESULT.txt` and stop. Full script is in the session scratchpad
(`scratchpad/sshd_crash_hunt.py`); it shells out to `overlays/devbox/run-smoltcp.sh` with
`SMP=4`.
