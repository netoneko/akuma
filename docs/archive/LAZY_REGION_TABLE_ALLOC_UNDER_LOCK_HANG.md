# `LAZY_REGION_TABLE` alloc-under-lock — a new rule-2 hang site (self-host `-j4`)

**Date:** 2026-08-02. **Build:** `release-smp-shared` + `devbox-smoltcp`, MEMORY=4096,
HVF, HEAD `5ea6024`. **Status:** FIXED 2026-08-02 by fix direction (3) below —
`LAZY_REGION_TABLE` was deleted and the per-pid map moved onto
`Process::lazy_regions: Spinlock<LazyRegionMap>`, so the lock is no longer global
and the teardown paths no longer re-acquire it. See §10 for what landed.

The original diagnosis is preserved verbatim below. Note it was never confirmed
live: the site and the deadlock chain were proven by static audit, but the wedged
instance was not booted with `GDB=1`, so the spinning PC was never captured. The
fix is structural (it removes the *class* for this subsystem regardless of which
route hung that particular box), but §6's route-A-vs-route-B question was closed
by construction rather than by evidence.

This is a sibling of [`EXECVE_STACK_LEAK_OOM_HANG.md`](EXECVE_STACK_LEAK_OOM_HANG.md)
§4 — same defect *class* (rule-2: `return_to_kernel*` re-entering a lock held by
the frame it abandons), different *lock* (`LAZY_REGION_TABLE`, not `PIPES` /
`SHARED_L0_TABLE`). The class is documented in
[`../reference/subsystems/thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md)
§4 rule 2 and §5; this doc adds the instance.

## 1. Symptom

First `-j4` self-host `cargo build -p akuma` in the session (the `-j1` run had
already pulled in 40+/147 crates cleanly over many minutes). ~5 minutes in, the
kernel serial log went completely silent for 30+ s of repeated polling while the
QEMU host process sat at 99-100% CPU. No `[OOM]`, no panic, no exception line.

This is exactly the "rule-2 spin" signature prescribed by `thread-lifecycle.md`
§Verify: *serial log frozen + 100 % CPU + no `[OOM]` line ⇒ rule-2 spin (attach
lldb to the gdbstub, the PC sits in a `Spinlock` spin loop).*

The instance was not booted with `GDB=1`, so the live PC was not captured. The
diagnosis below is by static audit of the access pattern against the code paths.

## 2. The access pattern that fingers the site

The tail of the serial log (last ~150 lines, not pasted into the prompt but
summarised) shows two processes active concurrently right before the freeze:

- **pid=105** — a long burst of `[mmap] pid=105 len=… = 0x… (lazy, N regions)`
  interleaved with `(eager)` calls. The lazy-region count for the tgid climbs
  (104 → 119 lazy regions). I.e. it is making many back-to-back `mmap` syscalls,
  mixing small eager mappings (<`MMAP_EAGER_MAX_PAGES`=16 pages,
  `src/config.rs:328`) with large demand-paged lazy ones.
- **pid=109** — interleaved `[mprotect] pid=109 owner=109 addr=… prot=0x1` calls
  (`PROT_READ`), the RELRO/GOT read-only-ing pass of a link step.

The documented §5.1 demand-page-under-lock sites
(`syscall/{sync,msgqueue,timerfd,term,signal}.rs`) are all *user-copy* faults —
none of them map onto an `mmap`/`mprotect` burst. The §5.3 alloc-under-lock list
names `SHARED_L0_TABLE` insert (fork), `fds.table` clone (`close_all`),
`pipe_write` buffer growth, epoll/eventfd interest inserts, and process-table
scans — none of which is `mmap`/`mprotect` either. So whatever hung the box is
either an uncatalogued site, or one of the §5.1 sites hit incidentally by the
build's terminal/futex traffic. The `mmap`/`mprotect` traffic points squarely at
the `LAZY_REGION_TABLE` mutators audited in §3 below.

`-j1` never hit it because heap pressure stayed low enough that the `BTreeMap`
node / `String` clone allocations inside those mutators never failed. `-j4` puts
four rustc/cc/ld/collect2 processes in RAM at once; one of those inserts finally
OOMs *inside* the lock hold.

## 3. Root cause: the `LAZY_REGION_TABLE` mutators allocate under the lock

`LAZY_REGION_TABLE` is
(`crates/akuma-exec/src/process/table.rs:480`):

```rust
pub static LAZY_REGION_TABLE: Spinlock<BTreeMap<Pid, BTreeMap<usize, LazyRegion>>> = …;
```

Every mutator takes it inside `with_irqs_disabled`, so the lock is held with IRQs
masked. Two of those mutators are reached directly by `mmap` and `mprotect` and
**allocate on the heap while holding the lock**.

### 3a. `push_lazy_region_with_source` — the `mmap` path (pid 105)

`crates/akuma-exec/src/process/children.rs:790`:

```rust
pub fn push_lazy_region_with_source(pid, start_va, size, page_flags, source) -> usize {
    let len = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();                          // held + IRQs masked
        let regions = table.entry(pid).or_insert_with(BTreeMap::new);      // ALLOC (outer split)
        regions.insert(start_va, LazyRegion { start_va, size, flags: page_flags, source }); // ALLOC (inner node)
        regions.len()
    });
    len
}
```

Called from `sys_mmap`'s lazy arm (`src/syscall/mem.rs:343`). With the region
count climbing through ~100, almost every `insert` allocates a `BTreeMap` node;
`entry().or_insert_with(BTreeMap::new)` additionally allocates the inner map (and
splits the outer node) the first time a tgid is seen.

### 3b. `update_lazy_region_flags` — the `mprotect` path (pid 109), heavier

`crates/akuma-exec/src/process/children.rs:862`:

```rust
pub fn update_lazy_region_flags(pid, range_start, range_size, new_flags: u64) {
    let range_end = range_start + range_size;
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();                          // held + IRQs masked
        if let Some(regions) = table.get_mut(&pid) {
            let keys: Vec<usize> = regions.range(..range_end)              // Vec ALLOC under lock
                .filter(|x| *x.0 + x.1.size > range_start)
                .map(|x| *x.0).collect();
            for key in keys {
                let r_source = regions[&key].source.clone();               // String clone ALLOC
                … // partial overlap: remove + up to 3× regions.insert(...)
            }
        }
    });
}
```

Called from `sys_mprotect` (`src/syscall/mem.rs:769`). **Five** distinct
allocation sites in one critical section: the `Vec::collect`, the
`source.clone()` per touched region, and up to three `regions.insert(...)` on the
partial-overlap path. `LazySource::File { path: String, … }` is
`#[derive(Clone)]` (`crates/akuma-exec/src/process/types.rs:239`), so each clone
heap-allocates a new `String`. RELRO application over demand-paged ELF segments
is precisely the partial-overlap case that takes the expensive remove + 3-insert
path — i.e. pid 109's `prot=0x1` burst is the worst-case input for this function.

This is the heaviest offender and the best match for the observed traffic.

### 3c. Sibling sites (same class, not yet implicated but listed for the fix sweep)

`munmap_lazy_region_overlapping` (`children.rs:948`, `regions.insert` under
lock), `clone_lazy_regions` (`children.rs:1024`, `regions.clone()` +
`table.insert` under lock — reached from fork), and `propagate_lazy_regions_to_child`
(`children.rs:817`). `LAZY_REGION_TABLE.lock()` appears in **19** call sites in
`akuma-exec` (17 in `process/children.rs`, 2 in `process/mod.rs`); every writer
that mutates the `BTreeMap` under the hold is rule-2 fuel.

## 4. The deadlock chain (proven, not inferred)

`alloc_error_handler` (`src/allocator.rs:500`) unconditionally routes a failed
user-context allocation through `return_to_kernel`:

```rust
#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    crate::safe_print!(256, "\n[OOM] allocation of {} bytes failed (…) — killing process\n", …);
    if akuma_exec::process::current_process_shared().is_some() {
        akuma_exec::process::return_to_kernel(-12); // ENOMEM  — -> !
    }
    panic!(…);
}
```

`return_to_kernel` (`crates/akuma-exec/src/process/mod.rs:1344`) is `-> !` and, in
its teardown, calls `clear_lazy_regions(pid)` at three sites:
`mod.rs:1291` (`teardown_forked_process_thread_group`),
`mod.rs:1555` (`return_to_kernel`), and
`mod.rs:1717` (`return_to_kernel_from_fault`). `clear_lazy_regions`
(`children.rs:1012`) re-acquires the lock:

```rust
pub fn clear_lazy_regions(pid: Pid) {
    let count = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();   // ◄── re-enter
        …
    });
    …
}
```

So the call stack at the moment of the hang is:

```
sys_mmap (pid 105)                          OR  sys_mprotect (pid 109)
└─ push_lazy_region_with_source  /  update_lazy_region_flags    [children.rs:790 / :862]
   with_irqs_disabled {                                          // IrqGuard LIVE
     table = LAZY_REGION_TABLE.lock()                            // SpinlockGuard LIVE, IRQs masked
     … BTreeMap insert / Vec::collect / source.clone() …
       → alloc (heap) → OOM (the −j4 pressure event)
         └─ alloc_error_handler                                  [allocator.rs:500]  prints [OOM]
            └─ return_to_kernel(-12)                             [mod.rs:1344]  (-> !, never returns)
               └─ … teardown …
                  └─ clear_lazy_regions(pid)                     [mod.rs:1555 → children.rs:1012]
                     └─ LAZY_REGION_TABLE.lock()                 // SPIN — held by the frame above
```

Single core (or: holding core with IRQs masked) + non-reentrant `Spinlock` ⇒
permanent spin, 100 % CPU, frozen serial. The abandoned frame's guards are never
released because `return_to_kernel` is `-> !` and jumps to the terminal
`loop { yield_now(); }` rather than returning through the mutator's epilogue.

Note: `Process::drop` (`mod.rs:905`) does **not** touch `LAZY_REGION_TABLE` (it
only frees `dynamic_page_tables`), so the *only* re-acquisition of this lock on
the exit path is the explicit `clear_lazy_regions` in `return_to_kernel*` /
`teardown_forked_process_thread_group`. That makes `clear_lazy_regions` the
single chokepoint where the re-entrant deadlock fires.

## 5. Why `try_lock`-and-skip in `clear_lazy_regions` is necessary but NOT sufficient

The obvious tactical fix — make `clear_lazy_regions` use `try_lock` and skip on
failure (the lock is held by us, the entry dies with the address-space teardown
anyway) — does **not** actually unbreak the box, and the reason is the more
important finding of this audit.

`return_to_kernel` is a *diverging leaf* (`-> !`, one of the three abandoned-stack
leaves in `thread-lifecycle.md` §4). It never unwinds the mutator's frame, and
the thread-slot recycler (`cleanup_terminated_internal`) later frees that stack
**without running the abandoned `SpinlockGuard`'s destructor** — there is no
unwinding in this `no_std` kernel. Consequently the `Spinlock`'s internal lock
byte stays "locked" for the lifetime of the boot. `clear_lazy_regions` skipping
saves us *once*; the **next** `mmap`/`mprotect` that calls
`push_lazy_region`/`update_lazy_region_flags` → `.lock()` spins forever on the
dead guard. The lock isn't held, it is *wedged*.

Implication: any complete fix must do one of:

- **(A) Prevent the allocation under the lock** — so the OOM never happens there.
  This is the only route that needs no new mechanism. `BTreeMap::insert`'s OOM is
  uncatchable in stable `no_std` Rust (the `GlobalAlloc` contract jumps straight
  to `alloc_error_handler`), so "prevent" means *structurally* removing the
  `BTreeMap` mutation from the hold: either move the per-pid map into `Process`
  (owned, dropped inside the existing `Process::drop` tree with no second global
  lock), or switch the container to one that can be pre-sized and mutated without
  per-op allocation.
- **(B) Force-release the lock on abandon** — track held locks per thread, and in
  `alloc_error_handler` (before calling `return_to_kernel`) forcibly reset the
  held `Spinlock`'s lock byte. Needs held-lock tracking + an `unsafe`
  force-unlock on `spinning_top::Spinlock`. Safe *here* because the dying thread
  is the owner and `BTreeMap::insert` allocates before mutating, so the tree is
  left consistent (minus the failed entry) when `alloc_error_handler` fires.
- **(C) Make `alloc_error_handler` policy-split on "lock held"** — when a
  `Spinlock` is held by the current thread, do *not* call `return_to_kernel`
  (which can't complete); instead mark the thread kill-pending and return to the
  scheduler so the guard's frame is unwound by the normal recycle path. Needs
  the same held-lock tracking as (B).

(B) and (C) both depend on a per-thread/per-core held-lock mask maintained at the
`Spinlock` layer. The cheapest version that closes the *class* (not just this
instance, including the `el1_fault_recovery_pad` leaf whose ambient lock set is
truly arbitrary) is a wrapper `Spinlock` that records its holder and exposes
`force_release_if_held(tid)`, plus a policy split in `alloc_error_handler`.
Migrating only the locks in the `thread-lifecycle.md` §3 drop tree (and the §5.3
alloc-under-lock writers) is incremental — start with `LAZY_REGION_TABLE`.

The same logic re-applies to every §5.3 site: `try_lock`-and-skip in the
re-acquiring teardown call is a tourniquet that buys one syscall; only (A)/(B)/(C)
actually unwedge the lock.

## 6. The `[OOM]`-line question

`alloc_error_handler` prints `[OOM] … — killing process` *before* calling
`return_to_kernel` (`allocator.rs:503`), so a route-A hang (alloc-under-lock →
`return_to_kernel`) should be preceded by exactly one `[OOM]` line. The instance
report said "no `[OOM]`". Two reconciliations, not yet distinguished:

1. The `[OOM]` line printed just above the 150-line tail window summarised in the
   prompt (the raw `hang_tail_150.log` was not actually attached — the prompt
   still has the placeholder). Action: grep the **full** log for `[OOM]` before
   spending a repro cycle.
2. The freeze is not route A but the §5.1 **drain** route (route B in
   `EXECVE_STACK_LEAK_OOM_HANG.md` §4): a demand-page fault during a lock-holding
   user copy (`FUTEX_WAITERS` / `TerminalState` / etc.) runs
   `alloc_page_zeroed_user` → `drain_retired_under_pressure` → `drop(Box<Process>)`
   → re-acquires the held lock. The drain path prints no `[OOM]`.

The access pattern (`mmap`/`mprotect`, not futex/terminal) favours route A and
the `LAZY_REGION_TABLE` site. But the `[OOM]`/no-`[OOM]` distinction is the
single piece of evidence that would separate route A from route B, so it is
worth nailing down on the full log before repro. If route B, the held lock is one
of the §5.1 set, *not* `LAZY_REGION_TABLE`, and the fix is the §5.2 mitigation
(gate the lazy resolver behind the `copy_*_safe` fixup check, or force a no-drain
`alloc_page_zeroed` while a `copy_*_safe` window is open).

The stale comments that obscure this distinction (called out already in
`EXECVE_STACK_LEAK_OOM_HANG.md` §4 and `thread-lifecycle.md` §5.1) are still in
the tree: `src/pmm.rs:771-775` claims "every caller of this function is the EL0
fault handler, which holds neither `as_lock` nor the PMM lock here" — false,
because `try_resolve_el1_user_copy_lazy_fault` (`src/exceptions.rs:1832`, called
from `rust_sync_el1_handler` *before* the `copy_*_safe` EFAULT fixup) is also a
caller, and it inherits its caller's lock set.

## 7. Confirming the diagnosis (live)

There is no magic-keypress / SYSRQ thread-dump facility in this kernel (grep for
`THR_DUMP`/`sysrq`/`debug_key`/`thread_dump` returns nothing). The only live
snapshot route is the gdbstub: reboot with `GDB=1` (gdbstub on `:$((1234+INSTANCE))`),
reproduce with `-j4`, and when the serial freezes attach lldb:

```
lldb -b -o "target remote :1234" -o "bt" -o "register read pc" \
     -o "image lookup -a \$pc"
```

Expected for route A: PC inside `Spinlock::lock`'s CAS retry; backtrace
`clear_lazy_regions` ← `return_to_kernel` ← `alloc_error_handler` ←
(`BTreeMap::insert` / `Vec::collect` / `String::clone`) ←
(`push_lazy_region_with_source` / `update_lazy_region_flags`) ← `sys_mmap` /
`sys_mprotect`. If the backtrace instead shows `drain_retired_under_pressure` ←
`drop(Box<Process>)` reached from `try_resolve_el1_user_copy_lazy_fault`, it is
route B (§5.1 drain) and a different lock.

## 8. Relation to the documented rule-2 class

This is not a new defect *class* — it is the exact rule-2 violation of
`thread-lifecycle.md` §4 ("`return_to_kernel*` and the pressure drain may
re-acquire any §3 lock, so they must only run from contexts holding none of
them"). What is new is the *site*: `LAZY_REGION_TABLE`'s writers are not listed
in §5.3. The canonical home for them is a new §5.1b table in `thread-lifecycle.md`
(sibling of the §5.1 demand-page-under-lock table, since these are syscall-path
*writes*, not user-copy faults) plus a §5.3 cross-reference — both added
2026-08-02 from this doc. The §2 lifecycle-edge table is deliberately *not*
touched: it tracks thread/process *state-machine* transitions (fork/execve/exit),
and `mmap`/`mprotect` are VM syscalls, not lifecycle edges.

## 9. Fix direction

In order of effort/payoff (fuller rationale in §5):

1. **Tactical, one-line-per-call, stops the bleed but does NOT unwedge** — make
   `clear_lazy_regions` (and the sibling teardown re-acquisitions) `try_lock` and
   skip on failure. Buys exactly one syscall of progress; the lock is still
   wedged for the next mutator. Useful only as a stopgap while (2)/(3) land.
2. **Class fix (recommended)** — per-thread held-lock mask maintained at the
   `Spinlock` layer + `alloc_error_handler` policy split (route B/C above). Closes
   the whole rule-2 class including the `el1_fault_recovery_pad` leaf, which the
   current audit cannot make provably safe. Start by migrating only the §3
   drop-tree locks and the §5.3 writers; `LAZY_REGION_TABLE` first.
3. **Structural (durable)** — move the per-pid `BTreeMap<usize, LazyRegion>` into
   `Process` (owned, dropped inside `Process::drop`'s existing tree). Demand-page
   lookups already resolve the owning tgid first
   (`address_space_owner_pid_for_fault`), so they can read the leader `Process`'s
   in-process region map. This collapses `LAZY_REGION_TABLE` out of the §3 drop
   tree and out of rule-2's reach entirely. Bigger refactor; the right one.

The in-tree correct pattern to mirror for any alloc-under-lock fix that keeps the
global lock: `src/exceptions.rs:907-911` (`ensure_user_page_mapped` keeps the
`alloc_page_zeroed_user` call *outside* the `as_lock` hold, with a comment
explaining why), and `src/syscall/mem.rs:456` (the eager-`mmap` path keeps
`alloc_pages_zeroed` outside the `with_address_space` hold for the same reason).

## 10. What actually landed (2026-08-02)

Fix direction (3), the structural one. `LAZY_REGION_TABLE` is gone from
`process/table.rs`; its per-pid inner map became a newtype, `LazyRegionMap`, in
`process/children.rs`, held as `Process::lazy_regions: Spinlock<LazyRegionMap>`.

- **Teardown no longer re-enters.** `return_to_kernel`,
  `return_to_kernel_from_fault` and `teardown_forked_process_thread_group`
  dropped their `clear_lazy_regions` calls (each carries a comment saying why).
  The map is released by `Process::drop` on the existing reclaim path — no second
  lock acquisition anywhere on the exit path, which is what §4 identified as the
  single chokepoint.
- **`clear_lazy_regions` survives** only for the `sys_wait4`/`sys_waitid` zombie
  reap, which frees a *child's* map from the reaping parent's syscall context. It
  holds no lazy-region lock, so there is nothing to re-enter; and it is now only
  an optimization, since the map would drop with the `Process` regardless.
- **The pid-keyed API is unchanged in shape** (`push_lazy_region*`,
  `update_lazy_region_flags`, `munmap_lazy_regions_in_range`,
  `clone_lazy_regions`, `lazy_region_lookup*`) — each now resolves `pid` →
  `Process` via `lookup_process_shared` and takes only that process's lock. §3's
  observation that demand paging already resolves the owning tgid first
  (`address_space_owner_pid_for_fault`) is what makes this work.
- **`propagate_lazy_regions_to_child` changed signature**, from
  `(parent_pid, child_pid)` to `(&[LazyRegion], &Process)`. This one is a trap
  worth naming: `fork_process` builds the child as a local `Box<Process>` and
  calls `register_process` only at the very end, so a pid-keyed propagation is a
  silent no-op at the point fork needs it — reinstating the first-touch SIGSEGV of
  [`FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`](FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md).
  Both fork arms now merge into `new_proc` by reference, before registration.
  For the same reason both arms' `new_proc.lazy_regions.clear()` lines are gone
  (pre-refactor they reset a vestigial `Vec` field that nothing read), and the
  post-registration `clone_lazy_regions(parent_pid, child_pid)` is gone too — it
  would overwrite the leader's descriptors with the forking thread's own map.
- **`push_lazy_region` on an unregistered pid is now a silent no-op**, where the
  global table happily accepted it. Boot-suite tests that used synthetic PIDs had
  to start registering a real `Process` (`LazyTestProcess` in `src/tests.rs`) or
  they would assert on an empty map and pass vacuously.
- **Coverage.** `LazyRegionMap` is unit-tested on the host
  (`lazy_region_propagation_tests` in `process/children.rs`) — the propagation
  regression tests kept their intent and moved down a layer, since building a
  real `Process` on the host needs a page allocator the stub test runtime lacks.
  The ~20 boot-suite lazy-region tests in `src/tests.rs` still exercise the
  pid-keyed wrappers end-to-end.

Not addressed: fix direction (2), the per-thread held-lock mask +
`alloc_error_handler` policy split. That is still the only thing that closes the
rule-2 *class* for the remaining §5.3 globals and for the
`el1_fault_recovery_pad` leaf. This fix removes one site — the one with the
cleanest owner — and is the template for the rest.

## Background

- [`EXECVE_STACK_LEAK_OOM_HANG.md`](EXECVE_STACK_LEAK_OOM_HANG.md) — the
  2026-08-02 investigation that named the rule-2 hang class and listed the
  original §5.3 sites. This doc is the `LAZY_REGION_TABLE` instance of that
  class, found the same day from a different access pattern.
- [`OOM_KILL_DEFERRED_RECLAIM_GAP.md`](OOM_KILL_DEFERRED_RECLAIM_GAP.md) — why
  the pressure drain exists and why its call sites are lock-context-constrained
  (the same constraint that makes §5.1/§5.3 dangerous).
- [`../reference/subsystems/thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md)
  §4 rule 2, §5.1, §5.3, §Verify — the current-state description of the class;
  §5.3 / §2 should be amended with the `LAZY_REGION_TABLE` sites per §8 above.
