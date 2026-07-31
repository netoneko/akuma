# BKL Process-Management Carve-Out Audit — Phase 3 (clone/execve/fork_process)

Companion to [BKL_VFS_CARVE_OUT.md](BKL_VFS_CARVE_OUT.md) §16.5, which flagged
`clone`/`fork_process` as the next Phase 3 candidate without starting it. This
doc is the audit that §16.5 recommended as the right-sized next task.

> **SUPERSEDED IN PART, 2026-07-31.** §§1–8 are the original audit and its
> "no carve-out is possible" conclusion. **[§9](#9-the-carve-out-that-did-land--no-bkl-process-2026-07-31)
> is the carve-out that subsequently landed**, and it revises §2's step-4
> classification: the audit missed that the CoW fault handler already edits the
> *same* parent PTEs BKL-free under the address space's `as_lock`, so an inner
> lock did exist — fork simply wasn't taking it. Read §9 for what the code does
> now; read §§1–8 for why steps 5–8 are still fully BKL-held (that finding
> stands unchanged).

**Status of §§1–8: AUDIT COMPLETE — no carve-out implemented.** The audit's conclusion is
that **no step of `fork_process`, `clone`, or the remaining uncovered portion of
`execve` is safe to carve out from the BKL without either (a) first fixing the
open SMP=4 fork-corruption bug, or (b) introducing a new lock.** Neither is a
"guard-and-measure" cycle off the VFS playbook — they are prerequisites this
code path does not meet. Per §16.5's own framing, this is a legitimate outcome.

**UPDATE 2026-07-31: the fork-corruption bug has been VALIDATED AS FIXED.** The
three-mechanism fix combination (`LifecycleGuard` + DSB barrier + per-PA
`COW_FAULT_LOCK`) was confirmed by a fork-hammer at SMP=4: 3 boots × 10 rounds,
0 fault signatures. See
[`docs/runbooks/debug-smp-fork-corruption.md`](../runbooks/debug-smp-fork-corruption.md)
for the full validation report. Prerequisite (a) for a BKL carve-out is now
met; prerequisite (b) (a real process-table lock) remains the blocking item.

The single concrete finding that changed the picture from §16.5's read: the
**`LifecycleGuard` is no longer a no-op** (it was re-enabled as a per-thread
`disable_preemption` guard), and that guard is currently load-bearing for
correctness — which makes the BKL the *other* half of a two-mechanism
correctness envelope, not a redundant lock sitting on top of an inner one.

## 1. The audit question

For each step of `fork_process` / `clone` / `execve`, determine: does the state
it touches already have its own fine-grained lock (like the VFS carve-out's fd
table / ext2 superblock), making the BKL redundant? Or is it genuinely relying
on the BKL for cross-core correctness?

This is the exact same question every VFS conversion answered with "yes, there's
an inner lock" before dropping the BKL. The answer for process management is
different.

## 2. `fork_process` step-by-step (`crates/akuma-exec/src/process/mod.rs:1487–2192`)

The `[FORK-DBG] step1`..`step8` markers delimit eight logical phases. Each is
classified below as **inner-locked** (BKL-redundant, carvable) or **BKL-dependent**
(relying on the BKL for correctness, not carvable without a new lock).

### Steps 1–3: address-space + Process struct creation (`mod.rs:1524–1587`)

Creates a new `UserAddressSpace`, allocates a process-info frame, constructs a
`Box<Process>`. None of this state is visible to other cores yet — the child
thread is not spawned, the Process is not registered. The allocations go through
the kernel allocator (its own freelist locking).

**Classification: inner-locked** (for the parts that touch lock-protected
state). The allocator and `UserAddressSpace::new` have their own synchronization.
The new `Process` is private stack-local state until step 8.

However, there is no on-disk/on-wire work here — the whole phase is pure
allocation and struct initialization. There is nothing to carve out: no I/O
window to open, no contention to relieve. The BKL-held time is negligible.

### Step 4: CoW fork / eager copy (`mod.rs:1666–2083`)

This is the dominant BKL-held window (the one the profiler credits as `clone`
22.5%). It splits into two sub-paths:

#### 4a. CoW share (`mod.rs:1668–1882`) — **BKL-DEPENDENT, NOT CARVABLE**

For each mapped parent page: `cow_ref_inc(pa)` (increments the global CoW
refcount), `child_as.map_page(va, pa, child_flags)`, `child_as.track_user_frame`.
Then `demote_range_to_ro(parent_l0, …)` modifies the **parent's live L0 page
table** (RW→RO), followed by `flush_tlb_all()`.

State touched and its locking:

| state | lock | carvable? |
|---|---|---|
| `COW_REFCOUNTS` (`src/pmm.rs:820`) | `Spinlock<BTreeMap>` + `with_irqs_disabled` | **Partially** — the spinlock is cross-core-safe, and `with_irqs_disabled` prevents the local IRQ-deadlock. But see below. |
| Child's new address space (`map_page`, `track_user_frame`) | Child's `as_lock` (`Spinlock`) | **Yes** — private to the not-yet-published child, no cross-core visibility. |
| Parent's L0 page table (`demote_range_to_ro`) | **NONE** — direct PTE writes via raw pointer | **NO** — the parent's page table is live hardware state (TTBR0 may be active). No lock serializes these PTE edits against a concurrent reader. |
| Global TLB (`flush_tlb_all`) | **NONE** — broadcast TLB invalidation | **NO** — must be coherent before any core (including the parent) touches the demoted pages. |

The CoW refcount table has a spinlock, but the PTE demotion and TLB flush do
not. The demotion + flush is a **two-step protocol that must appear atomic to
any observer**: if another core reads a shared page between the PTE demotion and
the TLB flush, it may use a stale cached RW TLB entry and write through to the
shared frame, silently corrupting the child's snapshot. This is exactly the
class of bug the runbook's "Missing DSB barrier in `demote_range_to_ro`" fix
(`docs/runbooks/debug-smp-fork-corruption.md` hypothesis 4) addressed — and that
fix relies on the BKL being held to serialize the demotion against all EL1
readers.

#### 4b. Eager copy (`mod.rs:1883–2083`) — **inner-locked but not worth carving**

Pure page-by-page copy (`alloc_page_zeroed` + `copy_nonoverlapping` +
`map_page`) into the child's private AS. No parent mutation. The allocator and
child `as_lock` protect the shared state. But like steps 1–3, there is no
on-disk/on-wire work — this is CPU-bound memory copy, not I/O. Dropping the BKL
here would allow another core to enter EL1 but wouldn't relieve any I/O
contention; the profiler attributes the cost to `clone` because of the *wall-clock
duration* under SMP=4 load (the copy takes milliseconds), not because it's
blocking on a contended resource.

### Step 5: ProcessInfo write (`mod.rs:2097–2111`) — **BKL-dependent**

Maps `PROCESS_INFO_ADDR` in the child AS and writes the `ProcessInfo` struct.
The child AS is private (not yet published), but `map_page` on
`PROCESS_INFO_ADDR` interacts with the Go-binary edge case documented inline
(code_start == PROCESS_INFO_ADDR == 0x1000). This is fast and non-blocking — no
carve-out benefit.

### Step 6: context capture (`mod.rs:2116–2139`) — **BKL-DEPENDENT, NOT CARVABLE**

`get_saved_user_context(parent_tid)` reads `THREAD_CONTEXTS[parent_tid]` — the
global per-thread saved register file. This is an `UnsafeCell` with **no lock**
(`crates/akuma-exec/src/threading/mod.rs:1377–1385`); its safety comment
literally reads *"We're single-CPU, so no concurrent access is possible."*
Accessed with only `with_irqs_disabled`, which takes **no cross-core lock**.

This is the exact state the fork-corruption runbook's **hypothesis 2** ("⭐
Strongest concrete lead") identifies as the source of the "user PC = kernel
address" corruption signature: a `THREAD_CONTEXTS[tid]` slot being aliased or
overwritten. Dropping the BKL here would allow another core's EL1 code to write
a *different* `THREAD_CONTEXTS` slot concurrently — technically different
elements of the same `UnsafeCell`-backed array, which is unsound under Rust's
aliasing model without explicit synchronization.

### Step 7: child thread spawn (`mod.rs:2142–2165`) — **BKL-DEPENDENT, NOT CARVABLE**

`spawn_user_thread_initializing` claims a thread slot (CAS-guarded, OK),
`update_thread_context(tid, &child_ctx)` writes `THREAD_CONTEXTS[tid]` (same
unlocked `UnsafeCell` as step 6), `THREAD_PID_MAP.lock().insert(tid, child_pid)`
(`Spinlock`-protected, OK).

The thread is kept `INITIALIZING` — not yet schedulable — so this step's
cross-core visibility is limited. But the `THREAD_CONTEXTS` write has the same
aliasing concern as step 6.

### Step 8: register + mark READY (`mod.rs:2171–2191`) — **BKL-DEPENDENT, NOT CARVABLE**

`register_process(child_pid, new_proc)` publishes the child into the process
table (`PROCESS_SLOTS: [AtomicPtr<Process>]`). The slot-claim CAS is fine, but
the `&'static mut Process` handed out by `current_process()` /
`lookup_process()` / `for_each_process()` to **218+ call sites** is guarded only
by `with_irqs_disabled` — a local IRQ mask with **no cross-core lock**
(`crates/akuma-exec/src/process/table.rs:112–143`). The safety comment says
valid "while IRQs are disabled **or** no other thread can call
`unregister_process`" — a single-core statement.

`mark_thread_ready(tid)` is the **publication point**: after this, the child is
cross-core-visible and schedulable. This must be atomic with the registration
and context write above it — a peer core that schedules the child between
`register_process` and `mark_thread_ready` would find a registered process with
an uninitialized thread, or vice versa.

## 3. `clone` call path (`src/syscall/proc.rs:348–502`)

`sys_clone` → `sys_clone_pidfd` is the sole entry point for fork-like clone
(`sys_clone3` at `:504` delegates to `sys_clone_pidfd` at `:541`). It has three
arms:

1. **`CLONE_THREAD | CLONE_VM`** (`:379–395`): routes to
   `clone_thread`. This creates a new thread sharing the parent's address space
   — a separate code path with its own CoW-free semantics. Not audited here
   (it's thread creation, not process creation). It was not named by the
   profiler (`clone`'s 22.5% is the fork path).

2. **`CLONE_VFORK` or SIGCHLD fork** (`:406–496`): the fork path. Pre-inserts
   into `VFORK_WAITERS` (`with_irqs_disabled` + `Spinlock`), then calls
   `vfork_process` or `fork_process`. The `VFORK_WAITERS` insert and the
   `fork_process`/`vfork_process` call are intentionally ordered (comment at
   `:418–421`), but the fork body itself inherits the BKL from the syscall
   wrapper.

3. **Everything else**: `ENOSYS`.

There is no shared body between `sys_clone` and `fork_process` — `sys_clone` is
purely routing and vfork-waiter management. The entire BKL-held cost is inside
`fork_process`/`vfork_process`, which §2 covers.

The only carve-out-eligible state in `sys_clone` itself is `VFORK_WAITERS`
(`Spinlock`-protected, `with_irqs_disabled`). But the insert/remove is
nanosecond-scale; no I/O, no contention.

## 4. `execve` remaining uncovered portion (`src/syscall/proc.rs:544–695`)

`do_execve` has three `cfg`-gated file-read arms, only one of which drops the
BKL:

| build profile | file-read arm | BKL dropped? |
|---|---|---|
| `kernel_profile_size` (`:603–614`) | reads 256-byte shebang header via `read_at` | **No** (but irrelevant — size profile targets 4 MB RAM, not SMP contention) |
| `kernel_smp` multikernel (`:618–636`) | forwards cross-core via `secondary_forward_read_file` | **No** (marshals through BKL-protected bounce — must keep BKL) |
| `not(kernel_profile_size), not(kernel_smp)` / smp-shared (`:637–661`) | whole-file `read_file` | **Yes** — `exec_bkl_drop_enabled()` gates `dropped_window_open/close` around the read (`:647–652`) |

After the read, `do_execve` calls `replace_image(data, ...)` or
`replace_image_from_path(path, ...)` — both **fully BKL-held**.

### `replace_image` (`crates/akuma-exec/src/process/image.rs:29–118`) — **BKL-DEPENDENT, NOT CARVABLE**

The destructive window: `deactivate` → `self.address_space = address_space` →
`mmap_regions.clear()` / `lazy_regions.clear()` → repopulate. This is the exact
window the fork-corruption runbook's **hypothesis 1** ("⭐ the tell is strong")
identifies as the lead: *"If the exec'ing thread is preempted between the
clear/AS-swap and repopulation, and anything reads that `Process`, it sees a
half-built image → `checked 0 mmap_regions`."*

This window is `LifecycleGuard`-acquired (`image.rs:48`) to prevent involuntary
preemption, and BKL-held to prevent concurrent EL1 on other cores. Both are
needed: the `LifecycleGuard` prevents the preemption-mid-mutation that the
decisive experiment proved is the corruption mechanism, and the BKL prevents a
peer core's `for_each_process` / signal delivery / fault handler from observing
the half-built `Process` — all of which read `Process` through the process
table's unprotected `&'static mut Process`.

### `replace_image_from_path` (`image.rs:121+`) — **mixed: load phase carvable, destructive phase not**

This variant (used by the size profile) does its own ELF load +
potentially block I/O inside `load_elf_with_stack_from_path`, BEFORE
acquiring the `LifecycleGuard` at `:129`. The load phase allocates a fresh
address space and reads from disk — state that is not yet installed and has no
cross-core visibility. By the VFS playbook's rule ("wrap only the on-disk work"),
this phase is theoretically carvable.

**But**: (a) this path is only taken on the `kernel_profile_size` build, which
targets 4 MB RAM and does not run SMP — there is no BKL contention to relieve;
(b) `load_elf_with_stack_from_path` interleaves allocation and I/O in a way that
makes a clean guard boundary hard to define without reading the loader in full.
Not worth pursuing for the SMP contention campaign.

## 5. The `LifecycleGuard` finding — why the BKL is not redundant here

The fork-corruption runbook (`docs/runbooks/debug-smp-fork-corruption.md`) says
in its middle "Status" block: *"Tree state now: LifecycleGuard is a documented
no-op on every build."* **That statement is stale.** The current code
(`crates/akuma-exec/src/process/lifecycle.rs:84–90`) shows:

```rust
pub fn acquire() -> Self {
    #[cfg(kernel_smp_shared)]
    crate::threading::disable_preemption();   // ← ACTIVE, not a no-op
    Self { _no_send: core::marker::PhantomData }
}
```

The `LifecycleGuard` was **re-enabled** as a per-thread preemption-disable guard
(per the runbook's own 2026-07-21 evening update at the top, which takes
precedence over the earlier "no-op" text in the middle). It is acquired at the
top of `fork_process` (`mod.rs:1491`), `vfork_process` (`mod.rs:2219`), and
inside `replace_image`/`replace_image_from_path` (`image.rs:48,129`).

This changes the carve-out analysis fundamentally. The VFS playbook's model is:
"BKL is redundant because an inner lock protects the state." For process
management, the model is: "BKL + LifecycleGuard together provide a two-mechanism
correctness envelope, and **neither mechanism is redundant**":

- **LifecycleGuard** prevents *involuntary preemption* of the mutating thread
  (the mechanism the runbook's decisive experiment proved is the corruption
  cause — preemption-mid-operation exposing half-mutated state to non-lifecycle
  readers when the BKL is reconciled away at the preemption `eret`).

- **BKL** prevents *concurrent EL1 on other cores* from observing the
  half-mutated state (the mechanism the VFS carve-out relied on NOT being
  needed, because VFS state had inner locks).

Dropping the BKL from any step that touches process-table or `THREAD_CONTEXTS`
state would remove the second mechanism without adding an inner lock — exactly
the "don't invent a new coarse lock" anti-pattern from the playbook, but in
reverse: the playbook says "rely on existing inner locks"; here there are none.

## 6. Cross-reference: fork-corruption signatures vs. steps

The runbook documents three heterogeneous crash signatures. Each maps to a
specific step:

| signature | runbook hypothesis | step | carve-out risk |
|---|---|---|---|
| `ppid=0`, empty `mmap_regions` | 1: `replace_image` not atomic across preemption | `replace_image` destructive window | **Direct**: dropping the BKL here allows a peer core's `for_each_process` to see the half-built `Process`. This is the single most dangerous carve-out target. |
| User PC = kernel address (`0x4011d004` = `rust_sync_el0_handler_inner+0x0`) | 2: `THREAD_CONTEXTS[tid]` aliased/overwritten | step 6 (`get_saved_user_context`) + step 7 (`update_thread_context`) | **Direct**: dropping the BKL allows concurrent `THREAD_CONTEXTS` writes from different cores on the unlocked `UnsafeCell`. |
| `FAR=0x0`, valid busybox PC, `x0=0` | 4: cross-core CoW/TLB coherence | step 4a (CoW share + demote + flush) | **Fixed** (2026-07-21 DSB + per-PA lock), but the fix relies on the BKL serializing the demotion. Dropping it reopens the window. |

The corruption is not a "whole-function property" — it localizes to specific
steps. But every localized step is one that touches BKL-dependent state. There
is no step that is both (a) a significant BKL-held time and (b) touching only
inner-locked state. The nearest candidate (step 4b eager copy) is inner-locked
but CPU-bound, not I/O-bound — carving it would allow concurrent EL1 but relieve
no contention.

## 7. Conclusion

**No carve-out is implemented.** The audit's specific findings:

1. **`fork_process` has no carvable window.** Every step that consumes
   significant BKL-held time (step 4 CoW share/demote, steps 6–8 context +
   publish) touches state with no inner lock (`THREAD_CONTEXTS`, parent page
   tables, the process table's `&'static mut Process`). The BKL is the lock.
   The `LifecycleGuard` is the preemption shield. Both are load-bearing.

2. **`execve`'s remaining uncovered portion (`replace_image`'s destructive
   window) is the fork-corruption bug's #1 hypothesized site.** Dropping the
   BKL there is the single most dangerous change in the entire Phase 3
   candidate space — it would make hypothesis 1 ("half-built image visible to
   `for_each_process`") trivially reachable by any peer core.

3. **`clone`'s routing layer (`sys_clone_pidfd`) adds no carvable work.** It is
   pure flag parsing and `VFORK_WAITERS` management (already spinlocked). The
   BKL-held time is entirely inside `fork_process`/`vfork_process`.

4. **The `LifecycleGuard` being re-enabled (contradicting the runbook's "no-op"
   text) is a documentation drift that should be corrected** — see §8 below.

### What would unblock a carve-out here

Per the playbook's rule "Don't invent a new coarse lock" — the carve-out
becomes feasible only when one of these is true:

- **(a) The fork-corruption bug is fixed and validated.** If the
  preemption-mid-operation exposure is eliminated at the root (not just papered
  over by `LifecycleGuard`), the BKL's role shrinks back to "prevent concurrent
  EL1" — and the inner-lock audit can proceed against stable behavior, without
  the triage problem ("did my carve-out cause this, or is it the pre-existing
  corruption?") that §16.5 flagged.

- **(b) A real inner lock is added to the process table and `THREAD_CONTEXTS`.**
  This is Phase 3's original plan
  (`BKL_FINE_GRAINED_LOCKING_PLAN.md` §201–270: `PROCESS_TABLE_LOCK` + per-process
  `ProcessLock`). It was never built — the VFS carve-out succeeded *without* its
  planned lock hierarchy because VFS state already had locks, and process state
  does not. Building it is real design work (218+ `lookup_process` sites to
  refactor), not a guard-and-measure cycle.

Until one of those is done, the `clone`/`fork_process`/`execve` BKL-held time
is structural — it is the lock, not a redundant wrapper around one.

## 8. Documentation corrections

Two stale claims found during the audit, neither changing the audit's
conclusion:

1. **`docs/runbooks/debug-smp-fork-corruption.md` "Tree state now" block**: says
   *"LifecycleGuard is a documented no-op on every build."* The code
   (`lifecycle.rs:85–86`) shows it calling `disable_preemption()` under
   `cfg(kernel_smp_shared)` — it is **active**, not a no-op. This matches the
   runbook's own top-of-file 2026-07-21 evening update, which takes precedence.
   The "no-op" text in the middle is from an earlier same-day revision that was
   superseded. **Recommendation**: update the "Tree state now" block to match
   the top-of-file status and the code, or remove it (the top-of-file update is
   the authoritative version).

2. **`docs/reference/subsystems/locking.md` §271**: says *"fork_process and its
   caller src/syscall/proc.rs have no BKL-drop treatment at all — confirmed by
   grep."* This is accurate and remains so after this audit — no carve-out was
   implemented. No correction needed; this section will need updating when/if a
   carve-out lands. **(Done — see §9 below and the `no-bkl-process` table in
   `locking.md`.)**

---

## 9. The carve-out that DID land — `no-bkl-process` (2026-07-31)

**Status: IMPLEMENTED, opt-in behind the `no-bkl-process` Cargo feature.**

### 9.1 What §2.4a got wrong

§2's step-4a table says the parent's L0 page table has **"NONE — direct PTE
writes via raw pointer"** for a lock, and concludes the demote is not carvable.
That is an accurate description of `fork_process` as it was written, but it is
**not a property of the operation**, and the audit drew the wrong conclusion
from it.

The CoW fault handler edits *the very same parent PTEs* — `remap_current_user_page`,
`update_current_user_page_flags`, `map_page` — and it does so **with the BKL
already dropped** (`fault_bkl_drop_enabled()`, the M5b Stage 4a window). What
serializes those edits is `Process::as_lock`, taken as
`AsLockHold::new(&owner.as_lock)` at eleven sites in `src/exceptions.rs`. So the
parent's page tables *do* have an inner lock, of exactly the kind the VFS
playbook asks for. Fork was simply the one page-table mutator in the kernel that
never took it — it relied on the BKL instead, which is why the audit's grep for
"what lock protects this" came back empty.

Once `as_lock` is in the picture, step 4 fits the playbook's model after all:
*the BKL is redundant here, because the state already carries a finer lock.*

The audit's other findings are unaffected. Steps 6–8 (`THREAD_CONTEXTS`, the
process table's unprotected `&'static mut Process`, `mark_thread_ready` as the
publication point) have no inner lock and are **still fully BKL-held**. So is
`replace_image`'s destructive window (§4). The carve-out is step 4 only.

### 9.2 Why a process-table lock was NOT needed for this

The original Phase 3 plan (`BKL_FINE_GRAINED_LOCKING_PLAN.md` §201–270) made a
`PROCESS_TABLE_LOCK` + per-process `ProcessLock` — and the 218-site
`lookup_process` refactor behind it — a prerequisite (§7 "(b)" above). It isn't
one, for this window:

1. **The page-copy window never touches the process table.** It walks page
   tables and shares frames. The child `Process` is private stack-local state
   (`Box<Process>`, not registered until step 8). The parent `Process` fields it
   reads — `brk`, `memory.code_end`, `memory.stack_top`, `mmap_regions`,
   `memory.next_mmap` — are set at process creation / ELF load and are not
   mutated during a syscall; the parent is the current thread, inside this
   syscall, so it cannot be concurrently in `mmap`/`brk`/`exec`.

2. **The one process-table access in the window was hoisted out.** The
   sibling-thread mmap scan (`for_each_process`) *does* walk the table, so it now
   runs **before** the window opens, collecting into the local `Vec` it already
   used. Same for the `LAZY_REGION_TABLE` snapshot and
   `propagate_lazy_regions_to_child` (which mutates the table).

3. **A process-table lock would be the wrong shape anyway.** It would be a new
   coarse lock held across a milliseconds-long copy — the exact anti-pattern
   `locking.md` §"Don't invent a new coarse lock" warns about. Adopting the
   existing safe `with_process(pid, f)` API at 218 sites is worth doing on its
   own merits, but it is a separate refactor and must not be coupled to this.

### 9.3 The design

`ProcessBklGuard` (`crates/akuma-exec/src/process/bkl_guard.rs`), modelled on
`VfsBklGuard`: gated by `cfg(all(kernel_smp_shared, kernel_no_bkl_process))`,
runtime toggle `process_bkl_drop_enabled()` (default on, **latched at
construction** — §2.4 of the VFS doc), opening/closing the per-thread dropped-
window ledger via `bkl::dropped_window_open()/close()`. Scoped to the CoW
share/demote pass in `fork_process` and nothing else.

Three constraints shaped the rest, and each of them contradicts a detail of the
obvious "just take `as_lock` around the copy" sketch:

**(a) It must be the thread-group LEADER's `as_lock`.** `CLONE_THREAD` siblings
each get a *fresh* `Spinlock` in their own `Process` (see `fork_process`'s
struct literal) while **sharing one address space**. The fault handler resolves
its owner via `address_space_owner_pid_for_fault()` — TTBR0 → the non-shared
process owning that L0. A worker-thread fork (`pid != tgid`) that took
`parent.as_lock` would hold a lock no fault handler ever waits on, and the
window would exclude nothing. `fork_process` now resolves the same way; inside
fork the live TTBR0 *is* the parent's address space, so the two always agree.

**(b) The hold must be chunked, never spanning the copy.** `AsLockHold` masks
IRQs for its duration, and it has to: without the mask, a timer IRQ inside the
BKL-free window does an unconditional `enter_kernel()` hard-spin for the BKL
while this core holds `as_lock`, against a peer that holds the BKL and wants
`as_lock` in `munmap`/`mprotect` — the AB-BA wedge the network Phase 2
`PreemptGuard` fix exists to prevent. But masking IRQs across a
milliseconds-long page copy is equally unacceptable (`locking.md`: "mask per
*attempt*, never across an unbounded wait"), and `AsLockHold`'s contract also
forbids page-frame allocation inside the hold, which `map_page` does on every
page. Hence `FORK_AS_CHUNK_PAGES = 64`: bounded holds, with the allocating
child-side work outside them.

**(c) The PTE read, the `cow_ref_inc`, and the demote must be in ONE hold.**
Split them and this race is live: fork reads a PTE naming frame X; a peer's CoW
fault breaks X (`cow_ref_dec` → 0 → frame freed, VA remapped to Y); fork then
`cow_ref_inc`s the freed X and maps it into the child. The fault handler
performs its break under this same `as_lock`, so one hold serializes the two.
**This is why the demote was merged into the share pass** rather than left as
the separate second walk it used to be — and the merge is a correctness
improvement independent of the carve-out: share and demote are now atomic per
page instead of separated by the whole copy. It also deletes one full redundant
page-table walk of the parent.

So `cow_share_and_demote_range` (hoisted to module level so the boot self-test
drives the real code) is, per 64-page chunk:

| phase | lock | work |
|---|---|---|
| A | `as_lock` (leader's), IRQs masked | snapshot parent PTEs into a **pre-reserved** scratch `Vec`, `cow_ref_inc` each, `demote_range_to_ro`, `flush_tlb_range_all_asid` |
| B | none | `child_as.map_page` + `track_user_frame` (allocating; child is private) |

Phase A is allocation-free apart from `cow_ref_inc`'s `BTreeMap` insert — the
scratch buffer is reserved once, outside every lock, so the collect reuses
capacity instead of growing under an IRQ mask. (Heap allocation under `as_lock`
is already the status quo: the fault handler's `track_user_frame` at
`exceptions.rs:2780` does it. The rule `AsLockHold`'s doc states is about
*page-frame* allocation, whose OOM/reclaim path can re-enter the lock.)

The per-chunk TLB invalidate is new and is required *by the merge*: with the
demote interleaved into the share, the single trailing `flush_tlb_all()` would
leave a sibling core's stale RW TLB entry live for the whole copy instead of for
one chunk. 64 pages keeps it under `flush_tlb_range_all_asid`'s 512-page
full-flush threshold, so it stays a targeted `tlbi vaae1is` sweep. The trailing
`flush_tlb_all()` is kept as-is.

### 9.4 Lock order

`BKL > as_lock`, unchanged and uninverted. Peers hold BKL → `as_lock`
(`munmap`/`mprotect`/`mmap`). Fork holds *no* BKL → `as_lock`. The fault handler
holds no BKL → `as_lock`. No cycle. The IRQ mask inside each hold is what keeps
it that way (constraint (b)).

### 9.5 What stayed outside the window

- Steps 1–3 (AS creation, process-info frame, `Box<Process>`) — pure allocation,
  no contention to relieve, and no I/O to overlap.
- Steps 5–8 — genuinely BKL-dependent, per §§2.5–2.8. Unchanged.
- The `for_each_process` sibling scan, the `LAZY_REGION_TABLE` snapshot, and
  `propagate_lazy_regions_to_child` — hoisted to just before the window (§9.2).
- **The eager-copy (non-CoW) branch** — `COW_FORK_ENABLED` is `true`
  (`src/config.rs`), so it is unreachable on every shipping build; and unlike the
  CoW path it copies page *contents* out of the parent, which would need
  `as_lock` held across each 4 KiB copy (not just the PTE read) to be safe
  against a peer's CoW break freeing the source frame mid-copy. Auditing that for
  a path nothing runs fails the "scope narrowly" test.

### 9.6 Incidental fix: leaked `FORK_IN_PROGRESS`

`fork_process` set `FORK_IN_PROGRESS` with a bare `store(true)`/`store(false)`
pair, so any `?` early-return from the copy loop (OOM mid-fork) stranded it
`true` for the rest of the boot — the timer handler then logged `[TMR]` 10× as
often forever. Now a `ForkInProgressGuard` (RAII), declared *before* the
`ProcessBklGuard` so the flag is set BKL-held and cleared only after the BKL is
re-acquired. The flag has no correctness consumers (grep: `src/timer.rs` log
frequency only), so this is hygiene, not a bug fix.

### 9.7 Self-test

`test_fork_bkl_drop` (`src/process_tests.rs`). It deliberately does **not** drive
`handle_syscall(CLONE, …)` end-to-end the way `test_unlinkat` drives `UNLINKAT`:
a real fork needs the calling thread to have a saved EL0 context
(`get_saved_user_context`, step 6) and needs the live TTBR0 to *be* the forking
process's address space so `parent_l0` walks user tables. Neither holds for the
boot self-test thread — it is a kernel thread on the boot tables with no EL0
frame, so a `CLONE` there would fail at step 6 having exercised nothing, and
pointing `parent_l0` at the boot L0 would have it CoW-share the *kernel's*
mappings. Instead it covers the seams that are reachable:

1. Guard ledger balance, including the **latching** rule: a toggle flipped ON→OFF
   while a guard is live must still re-acquire on drop, and the OFF→ON mirror
   must not close a window it never opened.
2. The real `cow_share_and_demote_range` on a synthetic parent AS of
   `FORK_AS_CHUNK_PAGES * 2 + 5` pages (two full chunks plus a partial trailing
   one, so an off-by-one in the chunk loop shows as a dropped tail, a re-shared
   chunk, or a doubled refcount). Checks: child maps the *same* PAs (share, not
   copy), parent demoted to RO, child mapped RO, `cow_ref_get == 2` exactly, and
   per-page content preserved (each page carries an index-derived byte, so a
   page landing at the wrong VA is caught). Run twice, toggle on and off — same
   inputs must give the same outputs either way.
3. The OOM early-return path leaves the ledger balanced.

### 9.8 Verification — what was run, and what could not be

**Builds** — clean on all four relevant configs, plus clippy (`-D dead-code`,
pedantic) and the host test suite:

| config | guard |
|---|---|
| `cargo build --release` | compiled out entirely |
| `--profile release-smp-shared --features devbox-smoltcp,no-tests` | compiled out (feature off) |
| `… ,no-bkl-process` | **active** |
| `… ,bkl-profile,no-bkl-process` | active + profiler |

**Boot self-test at SMP=2** — `test_fork_bkl_drop` PASSED (`133 pages
shared/demoted x2 toggles`), full suite green. Compared against a same-config
baseline boot with the feature off:

| signal | `no-bkl-process` | baseline |
|---|---|---|
| suite | green, `Process Execution Tests Done` | green |
| PANIC / WILD-DA / DA-MISS | 0 / 0 / 0 | 0 / 0 / 0 |
| `[BKL] stuck` | 16 | 16 |

The 16 `[BKL] stuck` are pre-existing and identical on both sides — they come
from the NEON preemptive-scheduling stress test (two deliberately busy threads),
well before the fork test runs.

**Fork-hammer at SMP=2** — `scripts/validate_fork_smp.py`, 3 boots × 10 rounds ×
8 concurrent SSH connections, each doing 8 `busybox true` + 8
`busybox echo fork_ok_<i>`:

| | `no-bkl-process` | baseline |
|---|---|---|
| verdict | **PASS** | PASS |
| fault signatures (SIGSEGV / WILD-DA / DA-MISS / PANIC / `ppid=0`) | **0** | 0 |
| children with partial/garbled output | **0** | 0 |
| boots clean | 3 / 3 | 3 / 3 |
| sshd session teardowns (pre-existing, not faults) | 43 | 33 |
| rounds lost to sshd exhaustion | 0 | 4 (boot 1, rounds 7–10) |

Every connection that completed returned **all 8** child markers — i.e. each
forked child executed correctly and its CoW-shared text/data/stack pages
resolved, which is the data-integrity bar rather than "exited 0".

**NOT run: the SMP=4 fork-hammer and the `bkl-profile` A/B.** Both drive the
workload over SSH, and SSH into the VM wedges at SMP=4 — *independently of this
carve-out*. Measured, same boot recipe, single connection:

| | `no-bkl-process` | baseline (BKL-held fork) |
|---|---|---|
| SSH connect at SMP=4 | times out (75 s) | times out (75 s) |
| `[BKL] stuck` during the attempt | 1842 | 1848 |
| PANIC / WILD | 0 / 0 | 0 / 0 |
| the same connect at SMP=2 | succeeds, `fork_ok_1` + `OK` | — |

So it is a pre-existing SMP=4 SSH wedge, present with fork fully BKL-held, and
the carve-out neither causes nor worsens it (marginally fewer stuck events). It
blocks the two remaining verification steps until it is root-caused separately.
Note the listener involved is the **in-kernel** SSH server (banner
`SSH-2.0-Akuma_0.1`), not the userspace `/bin/sshd`
(`SSH-2.0-Akuma_0.1_User`) — on a `devbox-smoltcp` boot the in-kernel server has
bound guest :22 long before herd's 10 s `start_delay_ms` elapses, even though
the `userspace-sshd` feature is documented as compiling it out. That mismatch is
its own pre-existing issue.

**Harness fixes made along the way** (`scripts/validate_fork_smp.py`), all
pre-existing bugs that made the harness unable to validate anything:

1. **Wrong port.** It polled `2323 + 100*INSTANCE` (the *tel* forward, guest :23)
   believing the devbox sshd binds :23. It binds :22 (`/bin/sshd --port 22`), so
   the harness waited 120 s on a port that was never open. Now `2222 + 100*INSTANCE`.
2. **Disk/memory env ignored.** It exported `DEVBOX_DISK`/`DEVBOX_MEMORY`, which
   are `run-smoltcp.sh`'s variable names — `cargo_runner.sh` reads `DISK`/`MEMORY`.
   Every "devbox at 4 GB" run in its history actually booted `disk.img` at the
   256 M default. Now `DISK`/`MEMORY`, both overridable.
3. **False-positive fault detection.** `src/tests.rs` prints a bun-install
   memory-requirements banner containing the literal line
   `[Fault] Process N (name) SIGSEGV after Xs`. The harness matched "SIGSEGV" and
   declared a crash on every boot before the hammer even started — which is
   exactly what the stale `fork_hammer_result.txt` in the tree recorded. Now
   filtered by `FAULT_FALSE_POSITIVES`.
4. **`FEATURES` is now overridable**, so the same harness can validate a carve-out
   build instead of only the default feature set.
5. **Data-integrity check added** (`verify_round`): children must produce correct
   output, not merely exit 0. It distinguishes *partial* output (real corruption:
   some pages resolved, some didn't) from *zero* output with the trailing `OK`
   present (the documented sshd session teardown under 8 concurrent connections —
   measured strictly all-or-nothing, and at the same rate BKL-held as BKL-free).

### 9.9 Feature wiring

`no-bkl-process` is **not** in the `smp-shared` feature set, unlike
`no-bkl-network`/`no-bkl-vfs`. Fork/CoW is the code path the SMP=4 corruption bug
lived in, so it stays opt-in until it has comparable soak:

```
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests,no-bkl-process
```

Unlike the other two carve-outs, the cfg has to reach `akuma-exec` (the guard is
constructed inside `fork_process`, not in a bin-crate syscall wrapper), so
`crates/akuma-exec/build.rs` emits `kernel_no_bkl_process` too. The runtime
toggle's atomic lives in `akuma-exec` for the same reason;
`src/smp_shared.rs` re-exports the accessors so all BKL toggles stay reachable
from one module.
