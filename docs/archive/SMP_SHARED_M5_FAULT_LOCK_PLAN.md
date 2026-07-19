# M5b plan — BKL-free user page-fault path (per-AS lock)

Design proposal. **No code lands until this is approved.** Extends the real
(shared-kernel) SMP effort (`cfg(kernel_smp_shared)`); see
[`reference/subsystems/smp-shared.md`](reference/subsystems/smp-shared.md) and the log
[`archive/SMP_SHARED.md`](archive/SMP_SHARED.md).

## Goal & evidence

Cut the SMP=4 coarse-BKL contention. A live SMP=4 self-test run (2026-07-19) showed all
four `smp_shared_*` tests PASS, with **102 transient `[BKL] stuck` events before the
pre-existing `test_mmap_file_oom` baseline panic, 99 of them `owner=1`** — one core
holding the BKL for ~10 ms chunks during FS/mmap-heavy work while the other three spin.
The hot contended path is demand paging (file-backed readahead + anon/CoW faults).

Target: **user page faults in *different* address spaces run in parallel** instead of
serializing on the BKL. Same-AS operations still serialize (correct).

## Why the naive versions are unsound (rejected)

- **"Drop the BKL, keep `&'static mut Process`"** — `map_page(&mut self)` relies on the
  BKL for `&mut` exclusivity. Two cores calling `lookup_process → &'static mut` is
  aliasing UB even if runtime-serialized.
- **"Drop the BKL around block I/O only"** — every other `BLOCK_DEVICE.lock()` taken
  *under* the BKL then deadlocks against the dropped-BKL holder. New deadlock class.
- **"Hold `vm_lock` across the fault"** — `vm_lock`'s documented rule is *never across
  alloc/yield*; a fault allocs + does block I/O. Wrong lock.

## Core idea

Introduce one **per-address-space lock** that serializes page-table mutation, held only
for **short** metadata/PTE windows (never across alloc or block I/O). The fault path
takes it **instead of** the BKL; the AS-mutating syscalls take it **in addition to** the
BKL they already hold.

Key simplification: **all syscalls still hold the BKL** (M5b removes the BKL only from
the fault fast path). So `as_lock` never arbitrates syscall-vs-syscall (the BKL already
does) — only **fault-vs-syscall** and **fault-vs-fault**. On the syscall side `as_lock`
is therefore uncontended except against a concurrent fault.

## The lock

New field on `Process` (thread-group leader owns the canonical one; CLONE_VM members
share the leader's, exactly like `fault_mutex`):

```rust
/// Serializes page-table mutation for this address space across cores under
/// shared-kernel SMP. Held only for short metadata/PTE-install windows with
/// preemption disabled — NEVER across alloc, block I/O, or a context switch.
/// Ordering: BKL > as_lock > {PMM, page_table_frames, user_frames, fault_mutex,
/// BLOCK_DEVICE, ASID_ALLOCATOR}. Faults take as_lock and NOT the BKL; AS-mutating
/// syscalls take the BKL (already) then as_lock.
pub as_lock: Spinlock<()>,
```

Keyed/looked up by `tgid` (same key the fault path already uses as `as_owner` and every
`mem.rs` syscall uses as `owner_pid`). Zero-cost/uncontended on non-`smp-shared` builds
(single core → always free; we can keep it always-compiled but only *rely* on it under
`cfg(kernel_smp_shared)`).

To avoid the `&mut` aliasing UB, add `lookup_process_shared(pid) -> Option<&'static
Process>` (atomic-load the `PROCESS_SLOTS[pid]` pointer, hand back a **shared** ref). The
fault path uses it; all AS mutations it needs are already `&self`
(`track_user_frame`/`track_page_table_frame` are `&self` + `Spinlock`; page-table writes
go through the free fn `mmu::map_user_page*`, which edits the *current* TTBR0 via raw
writes serialized by `as_lock`). CoW's one `map_page(&mut)` call switches to the
`map_user_page` free fn (or `map_page` is made `&self`).

## Lock ordering (global)

```
BKL  >  as_lock  >  { PMM, page_table_frames, user_frames, fault_mutex,
                      BLOCK_DEVICE, ASID_ALLOCATOR, LAZY_REGION_TABLE }
```

- Syscalls: BKL (whole excursion) → as_lock (short) → leaf locks. Consistent order.
- Faults: as_lock (short) → leaf locks. **Faults never take the BKL** on the fast path,
  so they can never invert BKL↔as_lock.
- Nothing ever takes as_lock then BKL. (Audited: the fault slow path below takes the BKL
  *instead of* as_lock, not nested.)

## Fault fast path (BKL-free) — three phases

Only the **resolvable demand-paging fault in our own `as_owner`** goes BKL-free. Split
so `as_lock` is never held across alloc or block I/O:

- **Phase A — decide `[as_lock + preempt-disabled, short]`**: look up the lazy region for
  `far`, classify (anon / file / CoW / PROT_NONE-commit), compute the readahead range and
  which pages need mapping. Release as_lock.
- **Phase B — prepare `[no as_lock, no BKL, long, parallel]`**: `fault_slot_acquire`
  (per-page, existing), alloc **private** frames (PMM lock), block I/O
  (`vfs::read_at*` → `BLOCK_DEVICE`), zero/fill, icache maintenance. Touches only PMM +
  private frames + block device + fault_slot — **no AS/process/VFS state** → sound to run
  with no as_lock and no BKL. This is the window that parallelizes across cores.
- **Phase C — install `[as_lock + preempt-disabled, short]`**: **re-validate** the lazy
  region still exists (a concurrent `munmap` may have removed it — matches BKL semantics
  where munmap-then-fault → SIGSEGV); for each prepared page still needed & unmapped,
  `map_user_page_no_flush` + `track_user_frame`/`track_page_table_frame` (shared ref);
  free unused/raced frames; batched `flush_tlb_range`. Release as_lock + fault_slot.

**CoW** (short, no block I/O): done entirely under as_lock (translate + `cow_ref_get` +
alloc + 4 KB copy + remap RW + `track` + `cow_ref_dec`). ~µs, fine to hold as_lock across.

**Anon single page / PROT_NONE-commit**: prepare the zeroed private frame in Phase B,
install in Phase C (or, being trivial, do both under as_lock).

## Fault slow path (keeps the BKL)

Take the BKL (as today, unchanged) for anything that isn't a clean self-AS demand page:
SIGSEGV / signal delivery, `fault_in_kernel_identity_user_range`, the DA-MISS diagnostic
that looks up a **foreign** `parent_pid` (a process that could be concurrently freed —
must stay BKL-guarded), OOM fallthrough, and the whole `EC_SVC64` syscall arm (JIT /
spurious-SVC guards, `record_el0_trap`). The wrapper decodes ESR first and routes:
BKL-free fast path for resolvable self-AS aborts, BKL for everything else.

## AS-mutating call sites to wrap with `as_lock` (+ preempt-disable)

Each already holds the BKL; add a **short** `as_lock` region around the page-table /
frame-tracking edits so a concurrent fault excludes correctly. Enumerated:

| Site | File:sym | Edits |
|---|---|---|
| `mmap` install | `src/syscall/mem.rs` `sys_mmap` (~229, 340, 375, 395) | map/unmap, track, update_flags, region push |
| `mmap` MAP_FIXED replace / mremap | `sys_mmap` (~442, 465, 470–498) | unmap+free, track, region push |
| `munmap` | `sys_munmap` (~663–679) | region detach, unmap+free |
| `mprotect` | `sys_mprotect` (~612–627) | `update_page_flags_no_flush` + flush |
| `brk` | `sys_brk` (~229 unmap / grow) | map/unmap |
| madvise/zero | `mem.rs` (~553, 573) | track, `zero_mapped_page` |
| stack grow | `process/mod.rs::alloc_and_map` (~557) | alloc+map |
| exec: old-AS replace | `process/mod.rs` (activate/replace) | AS swap |
| exit / group teardown | `mmu/mod.rs` `free_all` (~1026) + `process/mod.rs::teardown_forked_process_thread_group` (824) | drain user_frames + page_table_frames + free |
| CoW mark (fork) | fork path that marks pages RO + bumps `cow_ref` | update_flags, cow_ref |

Teardown (exit/exec) is the delicate one: it frees page tables. It must take `as_lock`
so an in-flight Phase C on a sibling core can't install into freed tables. Because a
group can't fully exit while a member thread is mid-fault (exit is delivered at
syscall/signal boundaries, not mid-fault), the leader Process stays alive for the
faulting thread; `as_lock` closes the sibling-core window.

## Preemption discipline

- Every `as_lock` hold runs with **preemption disabled** (`disable_preemption()` /
  RAII guard, the proven M5a `PreemptGuard` shape) so the lock is never held across a
  context switch → eliminates the "spinlock across switch" deadlock class entirely.
- Holds are short by construction (Phase A/C metadata + PTE writes; no alloc, no I/O), so
  the non-preemptible window is bounded and won't trip `check_preemption_watchdog`.
- Phase B (the long part) runs **preemptible** and lock-free of as_lock/BKL.

## Deadlock argument

1. `as_lock` is never held across a switch (preempt-disabled) → no cross-switch spinlock
   deadlock.
2. Faults never hold the BKL → a fault spinning on `as_lock` never blocks a BKL holder.
3. Syscall holds BKL+as_lock briefly (preempt-disabled) and has the CPU → it completes
   and releases as_lock; a same-AS fault then proceeds.
4. BKL↔as_lock ordering is one-directional (BKL always outermost; faults take neither
   inversion) → no ABBA.
5. Leaf locks (PMM/block/etc.) are never held while acquiring as_lock (as_lock is always
   outermost of the leaf set) → no inversion there.

## Process-table safety

- Table is a fixed `[AtomicPtr<Process>; 256]` — indices are stable under concurrent
  fork/exit of *other* slots.
- The fault fast path only looks up its **own** `as_owner` (alive for the fault's
  duration) via `lookup_process_shared` → no UAF.
- Any **foreign** lookup (DA-MISS `parent_pid` diagnostic) stays on the BKL slow path.

## Staging

1. Add `as_lock` field + `lookup_process_shared` + a `AsGuard` RAII
   (`disable_preemption` + `as_lock.lock()`), all no-op-equivalent on non-smp-shared.
   Wire it into the AS-mutating **syscalls** and teardown first, *while faults still take
   the BKL* — zero behavior change, pure scaffolding, verify no regression.
2. Convert the fault **anon + CoW + PROT_NONE** cases to BKL-free (Phases A/C; CoW under
   as_lock). Verify.
3. Convert the fault **file-backed readahead** case to BKL-free (Phases A/B/C). Verify.
4. (Follow-up M5c) revisit cross-core wakeup (`wake_remote_idle`) now that same-AS faults
   don't serialize on the BKL — it may stop being an anti-optimization.

## Test matrix (each gated build must pass before "done")

- Host: `cargo test` — `akuma-exec` (114) + `akuma-net` (32) + new `as_lock`/ordering
  unit tests; clippy clean on `smp-shared` / `devbox-smoltcp` / default.
- Boot self-tests `--features smp-shared`, `SMP=2` and `SMP=4`:
  `smp_shared_{cores_online,scheduler,userspace,migration}` PASS; **measure transient
  `[BKL] stuck` count before the baseline panic — expect a large drop vs the 102/99
  baseline** (this is the headline metric).
- New kernel self-test (`process_tests.rs`, per repo policy): two processes fault
  concurrently on distinct address spaces on distinct cores and both make progress with
  as_lock, not the BKL (assert BKL not held during the fault window).
- devbox-smoltcp: `SMP=2` boots to sshd + `ssh` round-trips with **0** `[BKL] stuck`
  (M5a baseline preserved); `SMP=4` boot + ssh, contention count recorded.
- Stress: `test_mmap_file_oom`-style file-mmap workload under `SMP=4` shows reduced
  contention (the measured hot path).

## Risk / rollback

Highest-blast-radius area in the kernel (page faults + munmap/exit). Everything is behind
`cfg(kernel_smp_shared)`; default / size / extreme / multikernel builds compile the same
bytes. Staged so each step is independently verifiable and revertable. If Phase 3
(file-backed) proves too racy, ship Phases 1–2 (anon/CoW BKL-free) and leave file faults
on the BKL as M5c.
