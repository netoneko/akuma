# BKL Process-Management Carve-Out Audit — Phase 3 (clone/execve/fork_process)

Companion to [BKL_VFS_CARVE_OUT.md](BKL_VFS_CARVE_OUT.md) §16.5, which flagged
`clone`/`fork_process` as the next Phase 3 candidate without starting it. This
doc is the audit that §16.5 recommended as the right-sized next task.

**Status: AUDIT COMPLETE — no carve-out implemented.** The audit's conclusion is
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
   carve-out lands.
