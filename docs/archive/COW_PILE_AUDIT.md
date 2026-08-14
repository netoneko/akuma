# The fork/vfork/clone + CoW pile: how many paths, and which ones can merge

**Date:** 2026-08-13. **Grade: B** — every line number, call site and reference
count below was read out of the tree on this date and is cited; the three
*defect candidates* in §5–§6 are reasoned from that reading and are labelled
**CONFIRMED** (mechanism established by code inspection) or **NEEDS-REPRO**
(mechanism established, consequence not yet observed in a log).

Written while doing Phase 6 item 5 of
[`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
(the `exceptions.rs` fault guards). The question that prompted it: *how many
process-duplication and CoW paths are there, and can they be merged or reduced to
one implementation each?*

Short answer: **the syscall layer is already merged and is the model to copy. The
duplication is one layer down — four copies of a 45-field `Process` literal, four
disagreeing CoW-break paths, and two ~330-line demand-paging bodies.** Three of
the disagreements are latent defects, and one of them is that a documented lock
does not lock.

---

## 1. The syscall surface is not the problem

AArch64 Linux has no `fork` or `vfork` syscall. There is `clone` (220) and
`clone3` (435), and libc synthesises the rest. Akuma implements exactly that:

```
  user: fork()      user: vfork()     user: pthread_create()   user: posix_spawn()
        │                 │                    │                      │
        └── musl ─────────┴────────────────────┴──────────────────────┘
                          │  clone(2) / clone3(2)
                          ▼
   ┌──────────────────────────────────────────────────────────────────┐
   │ src/syscall/proc.rs                                              │
   │                                                                  │
   │  sys_clone(…)   ──── 1-line delegation ────┐                     │
   │  sys_clone3(ptr) ── copy_from_user CloneArgs ──┐                 │
   │                     flags |= exit_signal      │                  │
   │                     stack += stack_size       │                  │
   │                                               ▼                  │
   │                      sys_clone_pidfd(flags, stack, …)  :430      │
   │                                  │                               │
   │      ┌───────────────────────────┼───────────────────────────┐   │
   │      │ flags>>32 != 0            │                           │   │
   │      │   → ENOSYS  :451          │                           │   │
   │      ├───────────────────────────┤                           │   │
   │      │ CLONE_THREAD|CLONE_VM     │ CLONE_VFORK               │   │
   │      │   :455                    │  or flags&0xFF == 0x11    │   │
   │      │                           │   :482                    │   │
   │      │                           │      │                    │   │
   │      │                           │      ├── vfork fastpath?  │   │
   │      │                           │      │   :509             │   │
   │      ▼                           ▼      ▼                    ▼   │
   │  clone_thread()          vfork_process()  fork_process()   ENOSYS │
   └──────────────────────────────────────────────────────────────────┘
```

Three entry points, one body, one dispatch. `sys_clone` is a two-line delegation
to `sys_clone_pidfd`; `sys_clone3` marshals `CloneArgs` and delegates to the same
place. **There is nothing to merge here** — this is what the rest of this document
is asking the other layers to look like.

Two notes on the dispatch that are easy to misread as bugs and are not:

- `flags >> 32 != 0 → ENOSYS` (`:451`) is deliberate. Go leaks `-ENOSYS`
  (`0xffff_ffff_ffff_ffda`) into the flags register in the vfork child, and those
  high bits coincidentally match `CLONE_THREAD|CLONE_VM`.
- `flags = 0` deliberately returns ENOSYS rather than routing to `fork_process`
  (`:477-481`): routing it creates a fork bomb, because each child re-runs the Go
  scheduler → `newosproc` → `clone`.

---

## 2. Three duplication primitives — and one 45-field literal written four times

> **MERGED 2026-08-14 (§8 items 3–4; the record is §8.2 below).**
> The three literals are now one `Process::inherit_from`; the shared tail is one
> `spawn_child_thread_and_publish`. Read §8.2 before trusting the two tables below:
> the divergence count is **six**, not nine (the three locks are identical at all
> three sites), the tail's `child_ctx.ttbr0` row is wrong for `clone_thread`, and
> the literal was written **six** times, not four.

| primitive | file:line | lines | address space | thread group |
|---|---|---:|---|---|
| `fork_process` | `process/mod.rs:2006` | 702 | **new**, CoW-shared from parent | new (`tgid = child_pid`) |
| `vfork_process` | `process/mod.rs:2708` | 186 | **shared** L0, new ASID | new (`tgid = child_pid`) |
| `clone_thread` | `process/mod.rs:2958` | 226 | **shared** L0, new ASID | parent's (`tgid = parent_tgid`) |
| `Process::from_elf` | `process/image.rs:294` | — | new, loaded from ELF | new |

`Process` has **45 `pub` fields** (`process/mod.rs:403-540`) and all four sites
write **all 45** as a struct literal. There is no builder, no
`..Process::inherit(parent)`, and no `Default`. Adding a field means editing four
literals, and the compiler only tells you that you missed one — never that you
gave it the wrong value for that path.

### What the three clone-family literals actually disagree about

Read across `:2054`, `:2726` and `:2998`. Of 45 fields, **36 are byte-identical
inheritance** (`pgid`, `name`, `entry_point`, `brk`, `initial_brk`, `memory`,
`args`, `cwd`, `stdin`, `stdout`, `spawner_pid`, `terminal_state`, `box_id`,
`namespace`, `channel`, `signal_mask`, all three `sigaltstack_*`, and the
constant-initialised remainder). Nine differ, and the difference is semantic in
six cases:

| field | `fork` | `vfork` | `clone_thread` | deliberate? |
|---|---|---|---|---|
| `tgid` | `child_pid` | `child_pid` | `parent_tgid` | **yes** — thread vs process |
| `address_space` | `new()` + CoW share | `new_shared()` | `new_shared()` | **yes** |
| `process_info_phys` | fresh frame | **parent's** | parent's | **yes** (identity via `THREAD_PID_MAP`) |
| `fds` | `Arc::new(clone_deep_for_fork())` | `Arc::new(clone_deep_for_fork())` | `parent.fds.clone()` (Arc) | **yes** — `CLONE_FILES` |
| `signal_actions` | fresh `SharedSignalTable` | fresh | `parent…clone()` (Arc) | **yes** |
| `clear_child_tid` | `0` | `0` | gated on `CLONE_CHILD_CLEARTID` | **yes** |
| `fault_mutex` | fresh | fresh | fresh | **suspect — see below** |
| `as_lock` | fresh | fresh | fresh | **suspect — see below** |
| `vm_lock` | fresh | fresh | fresh | **suspect — see below** |

**The three fresh locks on a shared address space are the interesting row.**
`fork_process` spends 14 lines (`:2116-2137`) explaining that `as_lock` is the
*thread-group leader's* lock, that a worker-thread fork taking its own
`parent.as_lock` "would hold a lock no fault handler ever waits on, and the window
would exclude nothing", and it resolves the owner through
`address_space_owner_pid_for_fault()` to avoid exactly that. Then `vfork_process`
and `clone_thread` both hand their child a **brand-new `as_lock` guarding an
address space it shares with its parent** — the very configuration that comment
describes as excluding nothing. It is survivable only because every fault-path
caller resolves the lock through `as_owner` rather than through `self`, which
makes the child's copy dead weight rather than a bug — until a caller forgets.
§5 is what happens when one does.

### The shared tail: ~40 lines, three times

After the literal, all three run the same closing sequence:

```
  capture parent ctx ── get_saved_user_context(parent_tid)
  child_ctx = parent_ctx; x0 = 0; spsr = 0
  child_ctx.ttbr0 = <own AS>.ttbr0()     ← the stale-ttbr0 fix, explained 3× (2618/2785/3055)
  child_ctx.sp = stack (if given)
  spawn_user_thread_initializing(entry_point_trampoline)
  new_proc.thread_id = Some(tid)
  THREAD_PID_MAP.insert(tid, child_pid)   ← under with_irqs_disabled
  sigaltstack: copy from parent  ─────────┐  fork ✓  vfork ✓  clone_thread ✗ (deliberate, :3073)
  update_thread_context(tid, &child_ctx)  │
  ProcessChannel::new() + register_channel│
  register_child_channel(…)  ─────────────┤  fork ✓  vfork ✓  clone_thread ✗ (deliberate, :3091)
  register_process(child_pid, new_proc)   │
  seed_thread_signal_mask(tid, …)         │  ← POSIX comment duplicated verbatim 3×
  mark_thread_ready(tid)                  ┘
```

`fork_process`'s and `vfork_process`'s tails are near-verbatim, including a
17-line POSIX signal-mask comment reproduced word for word at `:2675` and
`:2819`. `clone_thread` diverges twice, both times deliberately and both times
documented. **This tail is the single best merge candidate in the pile** (§8).

---

## 3. `COW_REFCOUNTS` is not a CoW refcount — it has two independent producers

This is the structural fact that makes the CoW paths hard to reason about, and it
is not written down anywhere except as two separate local comments.

```
                    ┌────────────────────────────────────────┐
                    │  COW_REFCOUNTS: BTreeMap<pa, u16>      │
                    │  src/pmm.rs:1497                       │
                    └────────────────────────────────────────┘
                          ▲                        ▲
      producer A          │                        │        producer B
  ┌───────────────────────┴──────┐   ┌─────────────┴─────────────────────┐
  │ fork CoW share               │   │ file_page_cache                   │
  │ cow_share_and_demote_range   │   │ src/file_page_cache.rs            │
  │   process/mod.rs:265         │   │   insert() :210, lookup_and_ref()  │
  │                              │   │   :137                            │
  │ RULE: one ref per ADDRESS     │   │ RULE: refcount = 1 (cached)       │
  │ SPACE. First share inserts 2  │   │        + 1 per MAPPING            │
  │ ("parent + child"); a frame   │   │ A second VA in the same AS hands  │
  │ mapped at several VAs still   │   │ its surplus ref back through      │
  │ gets exactly one.             │   │ drop_surplus_shared_ref (:1042)   │
  └──────────────────────────────┘   └───────────────────────────────────┘

  decrement side (all of it):
    pmm::free_page              :1174   ← the ordinary route; declines to free if refs remain
    ensure_cow_page_writable    exceptions.rs:1100  ┐
    try_resolve_el1_cow_fault   exceptions.rs:2410  ├─ the 3 CoW-break paths, each
    EL0 data-abort CoW break    exceptions.rs:3571  ┘  gated on `released_last_va`
```

Two accounting rules on one table. They coexist because both are "one reference
per holder that will eventually call `free_page` once" — but "holder" means
*address space* to producer A and *mapping* to producer B, and the reconciliation
between them lives in a single 6-line helper (`drop_surplus_shared_ref`) whose
correctness argument is in a comment above it. §5.6 of the trimming doc is the
record of what happens when one of the three decrement sites gets the rule wrong:
a refcount underflow that took three fixes.

---

## 4. Four CoW-break paths, and they disagree about their invariants

Every path does the same five things: translate the VA, check the refcount,
allocate a frame, copy 4 KiB, re-map RW and drop one reference. They disagree
about almost everything else.

| | `ensure_cow_page_writable` `:1050` | `try_resolve_el1_cow_fault` `:2350` | EL0 data abort `:3489` | `stale_write_fault_absorbed` `:2194` |
|---|---|---|---|---|
| trigger | kernel *about to* write user mem (pre-flight) | EL1 data abort (reactive) | EL0 permission fault | called first by the EL0 path |
| owner pid | `address_space_owner_pid_for_fault()` | **`read_current_pid()`** | `address_space_owner_pid_for_fault()` | n/a |
| absorbs a peer's win | no | no | **yes** | that *is* its job |
| per-page fault slot | no | no | **yes** (`CowFaultGuard`) | no |
| per-PA `cow_fault_lock` | no | no | yes — **but see §5** | no |
| refcount re-check after "lock" | n/a | n/a | yes `:3552` | n/a |
| 4 KiB copy is | **outside any lock** `:1075` | **outside any lock** `:2391` | **under `as_lock`** `:3554` | n/a |
| PTE write | `aspace.map_page` under `with_address_space` | same | `mmu::remap_current_user_page` under `AsLockHold` | n/a |
| TLB | `flush_tlb_page(va)` | `flush_tlb_page(va)` | inside `remap_current_user_page` | n/a |
| dec gate | `if released_last_va` | `if released_last_va` | `if released_last_va` | n/a |
| new flags | `RW_NO_EXEC` | `RW_NO_EXEC` | `RW_NO_EXEC` | n/a |

The bottom four rows agree — that is §5.6's three-times-applied fix holding. The
rows above it are where the drift is, and two of them are load-bearing.

### F1 — the EL1 paths copy the page outside the lock that makes the source valid
**CONFIRMED (mechanism) / NEEDS-REPRO (consequence). STILL OPEN — implemented and
backed out 2026-08-13. See F1b below for the mechanism that may make it unsafe as
prescribed.**

> **Attempt 1 (backed out).** The copy moved inside `with_address_space` in
> `complete_cow_break`'s `TakingAsLock` arm — §8 row 6 exactly. The VM then wedged in
> 3 of 4 SMP=1 exercise-suite runs: console output stops entirely, QEMU pegs one core,
> and it always happens at the transition *between* exercises where sshd forks the next
> one. One wedge had **zero** `[WATCHDOG] Time jump` lines on an idle host, so it is a
> spin with IRQs masked, not host descheduling.
>
> **It was backed out, and then the attribution collapsed.** With the copy moved back
> out, the same suite wedged again — 2 clean of 3 runs. So the wedge belongs to neither
> F1 nor F2; it is a pre-existing, intermittent (~40–50% per suite) hang in the
> exercise-suite path, and the same shape appeared once during the §8.1 merge cycle. The
> F1-applied numbers (1 clean of 4) versus F1-reverted (2 of 3) are **not** a
> measurement at that flake rate. F1 stays out only because there is no way to certify
> it while the background flake is louder than the signal.
>
> **Sequence for whoever picks this up:** characterize and fix the exercise-suite wedge
> *first* — it currently makes every SMP=1 exercise result a coin flip and blocks any
> fault-path change from being verified at all. Then re-apply F1 with F1b in mind, and
> measure over several runs per variant, not one.
>
> **On "needs measurement" (the hold-time question):** unresolved, and not resolvable on
> the development host. The existing benchmarks swing **4×** run to run there
> (`fork-cow-share` per_page 110 ns → 415 ns on identical trees), which is two orders of
> magnitude more noise than a 4 KiB memcpy, and no `[BENCH]` line exercises the EL1
> pre-flight path at all.

The EL0 path states the invariant explicitly (`exceptions.rs:3525-3527`):

> PTE overwrite + 4 KiB copy under `as_lock` (shared-kernel SMP): a concurrent
> munmap/fault on this AS is excluded, so **`old_pa` stays valid across the copy**.

Both EL1 paths do the copy *before* taking any lock —
`ensure_cow_page_writable:1075-1079` and `try_resolve_el1_cow_fault:2391-2395`
both `copy_nonoverlapping(phys_to_virt(old_pa), …, 0x1000)` and only then enter
`owner.with_address_space(…)`. So on shared-kernel SMP a peer core's munmap or CoW
break can free `old_pa` while the copy is reading it: a 4 KiB read from a frame
that is back on the PMM free list. That is a read of freed memory, and it is the
signature class of the open
[`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
defect — the copy would silently pick up quarantine poison
(`0xFEEDFACEDEAD0000 ^ pa`) or a recycled frame's contents.

The rationale for the invariant is filed under exactly one of the three copies.
This is the trimming document's central thesis — *the explanation lives under one
copy, and the copies that lack it violate it* — with memory corruption as the
cost rather than a lost log line.

### F1b — the EL1 paths still *translate* outside the lock
**CONFIRMED (mechanism) / NEEDS-REPRO (consequence). Found 2026-08-13 while fixing F1.
FIXED 2026-08-14 via option 1 below: `complete_cow_break`'s `TakingAsLock` arm
re-validates translate + refcount inside the hold and declines on a miss (the decline
path frees the unused frame and changes nothing — callers proceed/retry, which is
also what a slightly-later arrival would have seen). Boot test:
`cow_break_declines_stale_old_pa`.**

§8 row 6 prescribes moving "the 4 KiB copy inside `with_address_space`", and that is
what landed. It is not the whole invariant. Both EL1 sites still do this:

```
translate_user_va(fault/live ttbr0, page_va) -> old_pa   // outside as_lock
cow_ref_get(old_pa) > 0                                  // outside as_lock
alloc_page_zeroed()                                      // outside, and must stay out
    -> complete_cow_break(TakingAsLock(owner), ..., old_pa, ...)
       with_address_space(|aspace| { copy from old_pa; ... })   // NOW inside
```

So a peer can free `old_pa` between the translate and the acquire. The hold then
protects a copy from a frame that was already stale when the hold began — a narrower
window than F1's, in the same class, and the fix for F1 cannot close it. **The EL0 arm
does not have this**: it takes `AsLockHold` *first* and translates, re-checks the
refcount and copies inside it, which is why its comment claims the invariant honestly.

Two ways to close it, neither done here because both exceed "fix exactly F1 and F2":

1. **Re-validate inside the hold** — re-run `translate_user_va` in the closure and
   confirm it still names `old_pa` before copying, bailing otherwise. ~4 lines, no
   change to the locking structure, but it adds a decline path a caller must handle.
2. **Restructure the EL1 sites to pre-hold like EL0** — then they use
   `CallerHoldsAsLock`, `CowRemap` collapses to one variant, and the invariant holds by
   construction. This also swaps the PTE-write mechanism on both EL1 paths
   (`aspace.map_page` → `mmu::remap_current_user_page`), which is a fault-path change
   in its own right and wants its own cycle. Note `map_page` can in principle allocate
   intermediate page-table frames, which `with_address_space`'s own contract forbids
   inside the hold — a CoW break never hits that case (the leaf PTE exists), but option
   2 removes the latent contradiction as a side effect.

### F2 — `try_resolve_el1_cow_fault` resolves the wrong process
**CONFIRMED (mechanism) / NEEDS-REPRO (consequence). FIXED 2026-08-13.**

> Now `address_space_owner_pid_for_fault()`, matching its two siblings. No explicit
> `read_current_pid` fallback: that function already ends its own chain with it
> (`children.rs:1053`), so chaining another would be unreachable code in a fault
> handler. The mechanism — a shared view's `user_frames` map as a leak trap — is pinned
> by the boot test `test_cow_break_on_shared_view_leaks_both_frames`, which asserts both
> leaks against the wrong owner and their absence against the right one.
>
> The test pins the *mechanism*, not the call site: re-introducing `read_current_pid()`
> in the handler would not fail it. Guarding that would need a fault-path test with a
> live `CLONE_VM` thread, which the boot suite cannot construct.

`:2397` uses `read_current_pid()` where its two siblings use
`address_space_owner_pid_for_fault()`. For a single-threaded process these are the
same pid. For a **`CLONE_VM` worker thread they are not**: the worker has its own
`Process` slot and its own pid, while the address space (and every frame tracked
against it) belongs to the thread-group leader.

Three consequences, all verified against the implementations:

1. **`owner.with_address_space(…)` takes the worker's own `as_lock`** — the fresh
   `Spinlock` from §2's suspect row, which no fault handler ever waits on. The
   critical section excludes nothing; a concurrent EL0 CoW break on the leader's
   `as_lock` runs straight through it.
2. **`track_user_frame(new_frame)` lands on the worker's `user_frames` map.**
   `UserAddressSpace::new_shared` gives each sharer its *own* empty
   `user_frames: Spinlock<BTreeMap<…>>` (`mmu/mod.rs:641-647`) despite sharing the
   L0 table. And the `shared: true` branch of `Drop` (`mmu/mod.rs:1342-1375`)
   decrements the L0 refcount and **never frees its own `user_frames`** — the map
   is simply dropped. So the new frame is never freed: a leak for the life of the
   boot.
3. **`remove_user_frame(old_pa)` is called on a map that does not contain it**, so
   it returns `false` (`mmu/mod.rs:981`) and the `released_last_va` gate correctly
   suppresses `cow_ref_dec`. The old frame therefore keeps an elevated refcount
   forever: a second leak.

The direction of the error is *safe* — leak, not premature free — which is why
this has never crashed. But it is two frames leaked per kernel-side CoW break
taken by any threaded process, and threaded processes under kernel-side user
writes (futex wake, signal frame delivery) are the common case under load.

---

## 5. `cow_fault_lock` does not lock. It is a counter.
**CONFIRMED.** This is the headline finding.

The EL0 CoW path takes what it documents as the cross-PID serialization that the
per-page fault slot cannot provide (`exceptions.rs:3537-3541`):

> Serialize CoW break across parent/child processes that share this physical page.
> The per-PID `fault_slot` doesn't serialize across PIDs, so we need a **global
> per-PA lock** to prevent double-free races in the CoW protocol.

`src/pmm.rs:1547`:

```rust
pub fn cow_fault_lock(pa: usize) {
    // Increment the per-PA lock count
    crate::irq::with_irqs_disabled(|| {
        let mut locks = COW_FAULT_LOCK.lock();
        *locks.entry(pa).or_insert(0) += 1;
    });
}
```

It increments a counter and returns. It never spins, never waits, and nothing
anywhere reads the count. `COW_FAULT_LOCK` has **exactly three references in the
entire tree** — the `static` declaration (`pmm.rs:1492`) and the two counter
mutations in `cow_fault_lock` / `cow_fault_unlock`. Two cores breaking CoW on the
same PA both "acquire" it simultaneously; the count reaches 2 and both proceed.

So:

- The cross-PID serialization the EL0 path relies on **does not exist**. What
  actually keeps the refcount sound is the `released_last_va` gate — the §5.6
  fix — which is per-address-space bookkeeping and needs no cross-PID lock. The
  protocol is correct for a reason other than the one it documents.
- The "re-check the refcount after acquiring the lock" at `:3552` is not a
  barrier-protected recheck, just an opportunistic one. Harmless, but it reads as
  a guarantee.
- `CowFaultLockGuard` (`:3542`) is an RAII wrapper around decrementing a counter
  nobody reads.
- Of the four CoW function pointers registered into `ExecRuntime`
  (`src/main.rs:506-510`), **three are never called through it** —
  `cow_ref_inc` at `process/mod.rs:265` is the only live one. `cow_ref_dec`,
  `cow_fault_lock` and `cow_fault_unlock` are dead indirection.

**Do not "fix" this by making it a real lock.** A real per-PA lock in the CoW
fault path is a new cross-core serialization point on the hottest path in the
kernel, and the protocol does not currently need it. The correct resolution is to
**delete it** and move its stated purpose into the comment on the
`released_last_va` gate, which is what is actually doing the work. That is a
deletion of ~25 lines plus two runtime fields, and it removes a comment that
actively misleads anyone reasoning about this path — which, given §5.6, is the
expensive kind of comment to get wrong.

---

## 6. Two demand-paging bodies, ~330 lines, two behavioural divergences

Not CoW, but the same file and the same shape — and this is what §8 item 5 of the
trimming doc is actually pointing at (it names the `Drop` impls, which are 6 lines
each; see §7).

| arm | body | lines |
|---|---|---:|
| `EC_DATA_ABORT_LOWER` | `exceptions.rs:3708-4086` | 378 |
| `EC_INST_ABORT_LOWER` | `exceptions.rs:4337-4659` | 322 |

A `diff` of the two is 378 lines long and **almost all of it is comment drift**:
the DA arm carries the load-bearing rationale (why only whole pages are
shareable, why the readahead batch is clamped to `USER_PAGE_RESERVE`, why the BKL
is dropped for Pass B, why `ic ivau` uses the kernel VA) and the IA arm carries
one-line summaries that point back at it. Pass A / Pass B / Pass C are otherwise
the same algorithm with `frame_pool`/`pool_idx` renamed to
`ia_frame_pool`/`ia_pool_idx`.

Two differences are **behavioural**, and neither can be merged without choosing:

1. **`is_exec` gate vs hardcoded `true`.** The DA arm computes
   `is_exec = (map_flags & UXN) == 0` (`:3730`) and gates the I-cache maintenance,
   `file_page_cache::lookup_and_ref` and `insert` on it. The IA arm hardcodes
   `true` (`:4390`, `:4534`), justified as "every page reaching this arm is
   executable". That is *not* guaranteed: `map_flags = if flags != 0 { flags }
   else { RX }` (`:4357`), so a region whose recorded flags are non-exec maps
   non-exec and still claims `icache_done: true` into the shared cache. Merging on
   the DA rule changes IA behaviour; merging on the IA rule changes DA behaviour.
2. **`dsb ish; isb` placement in the single-page fallback.** DA emits it *before*
   the PTE install (inside `if is_exec`, `:4290`-ish); IA emits it *after*
   (`:4616`). Each has exactly one such pair, on opposite sides of the install.
   Note that **DA's single-page fallback disagrees with DA's own batch path**,
   which puts the barrier after Pass C — so this looks like a copy-paste artifact
   rather than a deliberate divergence, but "looks like" is not the bar for
   editing barrier placement in the fault path.

Per the `IrqGuard`/`isb` precedent in the trimming doc, the correct outcome for
(2) is *one body, two documented entry points* — not one behaviour — unless
someone measures it. **This merge is not recommended as hygiene work.** It is
~330 lines and it would be genuinely valuable, but it needs to be its own change
with its own SMP=4 verification, and the barrier question answered on purpose.

---

## 7. What Phase 6 item 5 actually is

§8 item 5 of the trimming doc says "`exceptions.rs` duplicated `Drop` impls
(`:3703` / `:4315`), ~142 lines". Corrections:

- There are **three**, not two: `CowFaultGuard:3511`, `DaFaultGuard:3716`,
  `FaultGuard:4348`. (The brief for this session already caught this.)
- Their `Drop` bodies are **byte-identical** — one call to
  `fault_slot_release(self.pid, self.page_va)`. All three are 6 lines. The real
  item 5 is ~24 lines including the identical `log_fault_reclaim` +
  `fault_slot_acquire` pair above each, not 142.
- The **~142 lines is measuring §6's demand-paging bodies**, which is where CPD's
  clone block starts (the guards are the token-identical anchor at the top of each
  region). Item 5 as written conflates a 24-line merge with a 330-line one.
- All three structs name the field `pid` and assign `as_owner` to it. The field
  has never held a pid.
- Only `FaultGuard` carries the rationale ("release the slot on ALL exit paths
  from this block, including early returns and fall-through"). Same
  filed-under-one-copy pattern as everything else here.
- `fault_slot_acquire`'s contract says to pair a *successful* (`Acquired` /
  `Reclaimed*`) return with exactly one release (`children.rs:496`), but all three
  sites release unconditionally, including after `NoProc`. This is benign —
  `fault_slot_release` is holder-gated — but the contract and the callers disagree.
- Latent, unrelated to the merge: a **re-entrant acquire on the same page by the
  same thread returns `Acquired`** (`children.rs:529`), so the inner guard's
  release removes the *outer* guard's entry and the outer release becomes a no-op.
  No known trigger; recorded because it is invisible from the call sites.

One more thing found in passing: `log_fault_reclaim`'s rustdoc
(`exceptions.rs:709`) opens with a sentence describing
`far_in_kernel_identity_user_range` — an orphaned `///` line that belongs to the
function 20 lines below it.

---

## 8. Can they be merged? Yes — in this order, and not all of them

Answering the question directly: **you cannot pick one implementation of
fork/vfork/clone_thread, because the three differ in what they share (§2's table
is six genuine semantic differences). What you can do is merge the parts that are
identical, which is most of them.** In risk order, cheapest and safest first:

| # | Merge | Size | Risk | Why now |
|---|---|---:|---|---|
| 1 | **`FaultSlotGuard`** — one guard + one `fault_slot_hold(pid, as_owner, page_va)` acquire-and-guard fn, replacing 3 structs, 3 impls and 3 log+acquire pairs | −24 | low | Phase 6 item 5 as literally scoped. Behaviour-preserving. Fixes the `pid`/`as_owner` misnomer and gives the release contract one home |
| 2 | **Delete `cow_fault_lock`/`unlock`** + `CowFaultLockGuard` + 2 dead runtime fields; move the stated purpose onto the `released_last_va` comment | −25 | low | §5. It is a counter nobody reads. Deleting it is strictly clarifying |
| 3 | **`spawn_child_thread_and_publish(...)`** — the ~40-line tail of §2, shared by all three primitives with `clone_thread`'s two documented opt-outs as parameters | −80 | medium | **DONE 2026-08-14 (§8.2).** Highest structural payoff in the pile. One place for the `THREAD_PID_MAP` ordering and the signal-mask seed that must precede `mark_thread_ready`. ~~the ttbr0 fix~~ — that one stayed per-site; §8.2 says why. Opt-outs turned out to be two enums **plus** a `before_ready` hook for three extra `clone_thread` steps |
| 4 | **`Process::inherit_from(parent, overrides)`** — one constructor, 36 inherited fields in one place, 9 explicit | −120 | medium | **DONE 2026-08-14 (§8.2).** Kills the four-45-field-literal problem (it was six literals). Done *after* 3, which was the right order. Not a `Default`: **7** mandatory override fields, not 9 — the three locks never differed. Measured **−26** net code lines, not −120; §8.2 explains why that is the wrong metric |
| 5 | **F2's owner resolution** (`read_current_pid` → `address_space_owner_pid_for_fault`) | 1 line | medium | A one-line fix to a two-frame-per-fault leak. Needs the boot suite at SMP=4 and a PMM drift check, not a merge |
| 6 | **F1's copy-under-lock** — move the 4 KiB copy inside `with_address_space` in both EL1 paths | ~10 | **high** | Extends a lock hold to cover a 4 KiB copy on the kernel-write path. Correct per the EL0 path's own invariant, but it is a hold-time change in the fault path and needs measurement |
| 7 | **The DA/IA demand-paging bodies** (§6) | −330 | **high** | Biggest number here and the most tempting. Do not do it as hygiene: answer the `is_exec` and barrier-placement questions first, on purpose, with SMP=4 evidence |

Items 1 and 2 are an afternoon and are pure clarification. Items 3 and 4 are the
ones that change how this code ages — after them, a new `Process` field is one
edit and the child-publish ordering has one definition. (Both landed 2026-08-14;
the "one edit" is `inherit_from`, plus `Process::from_elf`, which builds from an
ELF image rather than from a parent and so cannot share it — see §8.2.) Items 5–7 are defect work
wearing a refactor costume and should be scheduled as defect work.

**What must not be merged:** the three CoW-break *entry conditions*. The EL0 path
needs the stale-fault absorb and the per-page slot because it can race a sibling
on the same page; the EL1 pre-flight path (`ensure_cow_page_writable`) is called
*before* a kernel write and must be able to say "no CoW page here, proceed" in the
common case without touching a lock. Those are different jobs. The shareable part
is the middle — allocate, copy, remap, gate the decrement — which is ~25 lines and
is already in agreement on the part that matters (`released_last_va`).

---

## 8.1 The scoped "CoW merge" — decided 2026-08-13, **LANDED 2026-08-13**

> **Status: all three in-scope items are in the tree** (uncommitted at time of
> writing). What landed, and the four things this section got wrong:
>
> - **Item 1** → `complete_cow_break` + `CowRemap` in `src/exceptions.rs`, replacing
>   the middle of all three sites. The per-site divergence turned out to be *one*
>   thing, not two: whether the caller already holds `as_lock`. The copy's position
>   relative to the lock follows from that automatically (the helper copies first, so
>   it lands outside a lock the helper takes and inside a lock the caller already
>   holds), which is why F1 needed no separate parameter.
> - **Item 2** → deleted, including two `ExecRuntime` fields. The stated purpose moved
>   onto the `released_last_va` comment inside the new helper.
> - **Item 3** → `memmath::next_reclaim_step` / `ReclaimStep`, 6 host tests.
> - **This section said the escalation had FOUR steps; it has four *recovery actions*
>   plus give-up, behind five re-checks.** `ReclaimStep` therefore has 4 action
>   variants + `GiveUp` + `Allocate`. `memory.md` had it right ("six distinct recovery
>   mechanisms, not four" once `alloc_page`'s own two are counted).
> - **§9's F3 says "3 of 4 CoW runtime fn pointers are dead"; measured, it is 4 dead
>   of 5.** The five are `cow_ref_inc`/`dec`/`get`/`fault_lock`/`fault_unlock`, and
>   only `cow_ref_inc` is called through the table (`process/mod.rs:298`). Deleting
>   `fault_lock`/`unlock` leaves two still-dead fields (`cow_ref_dec`, `cow_ref_get`)
>   for `PMM_EXTRACT.md` to remove with the other 13. **DONE — `PMM_EXTRACT.md` step 5
>   landed in `eb19f23` and removed all 13**, these two included. `ExecRuntime` now has
>   no `cow_ref_*` field at all and `akuma-exec` calls `akuma_pmm::cow_ref_inc` directly
>   (`process/mod.rs:298`). Verified 2026-08-14: `grep -rn 'register_cow\|cow_ref.*hook'
>   src crates` returns only past-tense comments describing the deletion. There is
>   nothing left here to pick up.
> - **Two in-code comments cross-referenced `try_break_cow_for_kernel_write`, a
>   function that does not exist** in this tree under that name — the EL1 pre-flight
>   is `ensure_cow_page_writable`. Both cross-references are gone: they now point at
>   the one helper.
>
> **Cycle 2 (the "fix second" half) landed HALF, 2026-08-13**, as its own change on top:
> **F2's owner resolution only.** F1's copy-under-lock was implemented, wedged the VM,
> and was backed out — then the wedge turned out to reproduce without it too (F8, §9).
> Cycle 2 also turned up **F1b** (§4): moving the copy under the lock would not close
> the window anyway, because the *translate* is still outside it.
>
> The two-cycle split still paid for itself here. Because the merge had already landed
> and been verified, the wedge could be A/B'd against a known-good baseline within
> minutes, and the answer — "this is neither of your two fixes" — was reachable at all.
>
> ~~Items 3–4 of §8 (`spawn_child_thread_and_publish`, `Process::inherit_from`)~~ and item 7
> (the DA/IA merge) remain open, as scoped. **Items 3–4 landed 2026-08-14 — see §8.2.**

Two of §8's rows do not belong in a change called a CoW merge, and bundling them
would make it unverifiable:

- **Items 3–4** (`spawn_child_thread_and_publish`, `Process::inherit_from`) are
  *fork lifecycle* — 45-field literals and thread publication. They touch
  `fork_process` without touching one line of CoW break. Separate change.
- **Item 7** (the ~330-line DA/IA merge) is *demand paging*, a different
  subsystem that happens to share `exceptions.rs`, and it carries two
  behavioural divergences (§6) that must be decided deliberately. Separate change.

**In scope, as one change:**

1. The shared middle of the three CoW-break paths — allocate → copy → remap RW →
   `remove_user_frame` → gated `cow_ref_dec` — as one helper. Entry conditions
   stay per-site (§8's "what must not be merged": the EL0 path needs the
   stale-fault absorb and the per-page slot; the EL1 pre-flight must be able to
   answer "no CoW page here" without touching a lock).
2. Delete `cow_fault_lock` / `cow_fault_unlock` / `CowFaultLockGuard` and the two
   dead `ExecRuntime` fields (§5). Part of the same protocol cleanup.
3. `next_reclaim_step(free, done) -> ReclaimStep` — the pure decision behind
   `alloc_page_zeroed_user`'s four-step recovery escalation, host-tested, effects
   left in `src/`. Chosen over fully injecting the escalation because it captures
   the bug class that bites (a missing re-check between steps, the wrong order, or
   a premature `GiveUp`) without putting a fn-pointer call on the fault path
   (`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.11, "Still open here").

**Sequencing: merge first, fix second.** The helper lands strictly
behaviour-preserving — the EL1 paths keep `read_current_pid()` and the
outside-the-lock copy, passed in as parameters — and is verified by *nothing
changed*. A second change then flips both to the correct values (one owner
resolution via `address_space_owner_pid_for_fault`, one copy inside `as_lock`) and
is verified by *exactly these two things changed*. F1 and F2 then stop being
patches and become structurally impossible: with one helper the three paths cannot
disagree about the owner or the lock again.

The reason for two cycles rather than one: `cowstale`/`bssfork` and the BKL-stuck
line count are the only instruments here, and if the merge, the owner change and a
longer `as_lock` hold all land together, a regression in any of them is
unattributable.

## 8.2 The fork-lifecycle merge — §8 items 3–4, **LANDED 2026-08-14**

> **Status: both in the tree.** `crates/akuma-exec/src/process/mod.rs` only —
> `fork_process`, `vfork_process` and `clone_thread` now share one constructor and
> one publish tail. No CoW-break path, no `exceptions.rs` line, and none of
> F1/F1b/F2/F8's fixes were touched; this is the construction/publish half of the
> pile, exactly as §8.1 scoped it.
>
> - **Item 4** → `Process::inherit_from(parent, InheritOverrides)` (`mod.rs`,
>   in the `impl Process` block). One 45-field literal; the caller supplies a
>   struct with **seven mandatory fields** and no `Default`, no builder and no
>   `..rest` pattern, so an eighth divergence is a compile error at all three
>   sites rather than a silently wrong inherited value at two of them.
> - **Item 3** → `spawn_child_thread_and_publish(new_proc, &child_ctx, parent_tid,
>   ChildSigaltstack, ChildReaping, before_ready)`. The two `clone_thread` opt-outs
>   are two-variant enums rather than bools, so a future fourth primitive has to
>   state which contract it wants and cannot inherit a default.
>
> Done in §8's prescribed order (tail first, then the literal), which was the right
> call: with the tail already extracted, the literal merge was a pure field-by-field
> substitution with nothing else moving.
>
> ### What this section and §2 got wrong
>
> - **"Nine fields differ" is six.** Diffing the three struct literals
>   mechanically (strip comments, sort, `diff`) gives exactly six disagreements:
>   `tgid`, `address_space`, `process_info_phys`, `fds`, `signal_actions`,
>   `clear_child_tid` — §2's six "deliberate" rows, and only those. The three
>   flagged as **suspect** (`fault_mutex`, `as_lock`, `vm_lock`) are byte-identical
>   `fresh` at all three sites, so they are a *design* concern, not a divergence.
>   `inherit_from` therefore owns them rather than making them overrides, and the
>   argument for fresh-per-child — the fault path resolves the owner through
>   `address_space_owner_pid_for_fault()`, never through `self`, so a `CLONE_VM`
>   member's own copy is never the lock anyone waits on — now lives in one comment
>   next to the three of them instead of only under `fork_process`. §2's row stands
>   as a *question*; nothing about it changed here.
> - **`fork` and `vfork`'s literals differ in exactly ONE field**
>   (`process_info_phys`). §2's table implies `address_space` differs too; it does
>   not — both sites move a locally-built `UserAddressSpace` into the same field.
>   What differs is the *construction* above the literal (`new()` + CoW share vs
>   `new_shared()`), which stays at the call sites.
> - **§2's tail listing gets `child_ctx.ttbr0` wrong.** It records
>   `child_ctx.ttbr0 = <own AS>.ttbr0()` for all three. `clone_thread` does **not**
>   do that: it uses `parent.address_space.ttbr0()` (captured as `shared_ttbr0`
>   before `shared_as` is moved), which carries the **parent's ASID** — while
>   `new_shared()` gave the child its own new ASID. `fork`/`vfork` use their own
>   address space's `ttbr0()`. So the stale-ttbr0 fix is one *idea* explained three
>   times but not one *expression*, and child-context construction stayed at the
>   call sites. The shared tail begins at `new_proc.context = child_ctx`.
> - **`clone_thread` diverges three more ways than the "two documented opt-outs".**
>   It also runs three extra steps in the tail: `record_clone_snapshot` (the
>   thread-spawn SIGSEGV hand-off diagnostic), `clone_lazy_regions`, and the
>   `CLONE_PARENT_SETTID`/`CLONE_CHILD_SETTID` tid publication. These are handled
>   by a `before_ready(tid)` hook that runs after `register_process` and before the
>   signal-mask seed — the last window in which the child provably has not executed
>   an instruction. `fork`/`vfork` pass a no-op.
> - **Two orderings had to be picked, and the doc did not know they disagreed.**
>   (a) The `THREAD_PID_MAP` insert: `fork`/`vfork` do it immediately after
>   `thread_id = Some(tid)`; `clone_thread` did it *after* `register_channel`. The
>   merged tail uses fork's position. (b) `record_clone_snapshot` moved from
>   immediately-after-spawn into `before_ready`, i.e. after `register_process`.
>   Both moves are inside the INITIALIZING window where the child cannot run, and
>   nothing between the old and new positions touches the parent's address space or
>   the user stack the snapshot reads — but they are the only two behavioural
>   deltas in this change, so they are named here rather than buried.
> - **Two `[FORK-DBG]` trace strings are gone.** `step8: registering process` and
>   `step8: marking child READY` are replaced by `[FORK-DBG] child-publish: …`
>   lines emitted from the shared tail for all three callers (a `step8` printed
>   during a `pthread_create` would be a lie). `step7: spawning child thread` is
>   preserved verbatim at fork's call site.
>   [`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) §2's "`step1`..`step8`
>   markers delimit eight logical phases" should now be read as step1..step7 plus
>   `child-publish`.
> - **§2 says the 45-field literal is "written four times"; it was six.**
>   `fork_process`, `vfork_process`, `clone_thread`, `Process::from_elf`
>   (`image.rs`), **and two boot-test fixtures** (`src/tests.rs`,
>   `src/process_tests.rs`). Four remain. `from_elf` is deliberately not a caller
>   of `inherit_from`: it builds from an ELF image, not from a parent, so there is
>   nothing to inherit. The test fixtures are left alone on purpose — they exist to
>   fabricate *unusual* `Process` states, which is the one job `inherit_from` must
>   not make easy.
> - **§8's line estimates (−80 and −120) are wrong, and the metric is wrong.**
>   Measured: **+403 / −321**, i.e. **−26 net non-comment lines**. The estimate was
>   counting the duplicated *comments* as savings — the 17-line POSIX signal-mask
>   block written twice, the ttbr0 rationale three times — and those really were
>   removed, but what replaces them is longer, because one comment now has to
>   explain both contracts (why `fork` inherits a sigaltstack *and* why
>   `clone_thread` must not) where each copy previously explained only its own.
>   The duplication actually removed is ~135 struct-literal lines and ~80 tail
>   lines. **Judge this item by "how many places must change to add a `Process`
>   field" — was 6, now 4, of which 2 are test fixtures — not by line count.**

## 9. Findings summary

| id | finding | status | verify with |
|---|---|---|---|
| F3 | `cow_fault_lock` provides no mutual exclusion; 3 of 4 CoW runtime fn pointers are dead | **DONE 2026-08-14** — deleted with the `akuma-pmm` extraction ([`PMM_EXTRACT.md`](PMM_EXTRACT.md)); the last two dead `ExecRuntime` fields (`cow_ref_dec`, `cow_ref_get`) went with it. `cow_fork_enabled` is the only `cow*` name left in `runtime.rs` | `grep -rn COW_FAULT_LOCK src crates` → **0 hits** |
| F2 | `try_resolve_el1_cow_fault` resolves the owner with `read_current_pid()`; for `CLONE_VM` workers this leaks two frames per kernel-side CoW break and takes a lock nobody waits on | **FIXED 2026-08-13** (mechanism was CONFIRMED, consequence still NEEDS-REPRO) | PMM drift across a threaded workload taking kernel-side user writes; `[AS-*]` traces |
| F1 | both EL1 CoW-break paths copy 4 KiB from `old_pa` outside the lock the EL0 path documents as required for `old_pa`'s validity | **FIXED 2026-08-14** — copy moved inside `with_address_space`; unblocked by the F8 fix (F1 was F8's amplifier, §10.2, never the defect). Verified: 10/10 amplified + 3/3 unamplified clean suites | `cowstale`, `bssfork spread=1` at SMP=4; poison decode in `[WILD-DA]` |
| F8 | the SMP=1 exercise suite wedges intermittently: console output stops, one core spins at 100%, **zero** time-jump lines on an idle host. Root cause: the scheduler SGI installed a **freed** L0 into TTBR0 from a zombie thread's saved context — the page-table-UAF free gate only saw TTBR0s live on cores, never saved contexts (§10.1–10.2: `TTBR0_EL1` at the wedge == the last `[AS-FREE]` line's L0; ESR=0x86000004, recursive vector-fetch abort) | **ROOT-CAUSED + FIXED 2026-08-14** (§10.3: saved-context arm on the free gate + drain re-check; boot test `as_drop_defers_while_saved_ctx_on_l0`) | `scripts/f8_wedge_repro.py`; `[SGI-S FREED-L0]` must never print; `[AS-FREE-DEFER] ... held_by_ctx` lines are the gate working |
| F1b | the EL1 paths still translate the VA and read the refcount outside `as_lock`, so `old_pa` can be stale before the hold begins — the residue F1's prescribed fix cannot reach | **FIXED 2026-08-14** — §4 option 1: the `TakingAsLock` arm re-validates translate + refcount under the hold and declines the break (frees the unused frame, changes nothing) on a miss; boot test `cow_break_declines_stale_old_pa` | **The prescribed re-measurement was done 2026-08-14 and answered a different question: the cargo null-`Rc` was not this window.** It was `MADV_DONTNEED` zeroing a CoW-shared frame out from under the peer — [`MADV_DONTNEED_SHARED_FRAME.md`](MADV_DONTNEED_SHARED_FRAME.md). F1b remains a real narrowed window and its fix stands; it is simply no longer the null-`Rc` candidate, and nothing here is gated on that measurement any more |
| F4 | DA single-page fallback places `dsb ish; isb` before the PTE install, disagreeing with its own batch path and with the IA arm | **CONFIRMED** | inspection; no known symptom |
| F5 | IA arm claims `icache_done: true` into `file_page_cache` for pages it may map non-exec | **CONFIRMED** | inspection; no known symptom |
| F6 | re-entrant `fault_slot_acquire` on one page by one thread lets the inner release drop the outer's entry | **CONFIRMED** | inspection; no known trigger |
| F7 | survey errors: item 5 is three guards not two, ~24 lines not 142; `log_fault_reclaim`'s rustdoc opens with another function's sentence | **CONFIRMED** | §7 |

None of F1–F6 was being fixed in this document's original change. F1, F2 and
F4–F6 are recorded here because they are invisible from any single call site and
only surface when the copies are read side by side — which is the argument for
§8's items 3 and 4.

> **Status as of 2026-08-14: F1, F1b, F2, F3 and F8 are closed.** What is left is
> **F4** (barrier ordering in the DA single-page fallback), **F5** (`icache_done`
> claimed for possibly-non-exec pages) and **F6** (re-entrant
> `fault_slot_acquire`) — all CONFIRMED by inspection, none with a known symptom
> or trigger, all small. F7 is survey bookkeeping. Nothing in that set is blocked
> on a measurement any more.

## 10. F8 — the suite wedge, localized (2026-08-14)

**Status: ROOT-CAUSED and FIXED 2026-08-14 — see §10.1 below for the registers that
named the defect, §10.2 for the mechanism, §10.3 for the fix. The rest of this
section is the investigation record as it stood before the catch, kept verbatim;
its "inferred, not identified" caveats are resolved by §10.1.**

### What was measured

Caught under a gdbstub (`GDB=1`, port 1234) with `scratchpad/f8_repro.py`, which drives
the four exercises and attaches `lldb` against the symbolized ELF the moment the boot log
stops growing while QEMU is still alive:

```
pc  = 0x401c1200  akuma`exception_vector_table + 512   (7 of 7 samples — not advancing)
x30 = 0x402d15cc  akuma`threading::sgi_scheduler_handler_with_sp + 432
```

Vector offset **0x200 is entry 4 — EL1 synchronous, SP_ELx**. A PC pinned there across
every sample means a *fault loop*: the sync handler re-faults before it can advance. That
accounts for every symptom at once, and for why this looked like several different bugs:

| Symptom | Because |
|---|---|
| no console output at all | the fault re-raises before anything can print |
| one core at 100%, VM "alive" | it is spinning in the vector, not halted |
| zero `[WATCHDOG] Time jump` lines | IRQs are masked in the loop, so nothing measures lost time |
| guest clock frozen | same — no timer interrupt is ever taken |
| looked like host starvation | an unresponsive VM with a live QEMU is indistinguishable from a descheduled one from outside |

### The unrecoverable path that was closed

`sgi_scheduler_handler_with_sp` validated the incoming stack pointer as
`new_sp == 0 || new_sp < 0x4000_0000` — a floor test that accepts **any** garbage at or
above RAM base: a stale SP, a recycled slot's SP, one past `ram_end`, or a misaligned one.
Code further down then **dereferences it**: the `[SGI-S POISON]` tripwire reads
`new_sp + 240` and `new_sp + 248`, and the restore does `ldp` off it. On a bad SP either
one raises the EL1 sync fault above — and a diagnostic that hangs the machine is worse
than the corruption it was added to catch.

The check now requires, before any dereference: inside `[ram_base, ram_end)` (read from
the live window, not a hardcoded floor, since it moves with `MEMORY=`), at least a whole
256-byte restore frame below `ram_end`, and 16-byte alignment. Failure prints
`old_tid`/`new_tid`/the RAM window and halts the core as before — so the next occurrence
is a named, attributable line instead of silence.

An `[SGI-S FATAL] new_sp=0x0` sighting earlier the same day is the same defect with the
one value the old test happened to catch.

### What is NOT established

- **Which instruction faulted.** `x30` points into the handler and the PC into the vector,
  but `ELR_EL1`/`ESR_EL1`/`FAR_EL1` were not captured. The repro harness now dumps
  `register read --all` for exactly this; the next catch settles it.
- **The invalid-SP inference is REFUTED.** "The tripwire read faulted on an unvalidated SP"
  was the leading theory. With the guard in place the wedge **reproduced anyway** — full
  gate, 2026-08-14, SMP=1: `bssfork`/`forkprobe`/`elftest` all TIMEOUT after `cowstale`,
  zero time jumps, same `[AS-FREE] … path=owner` last line — and **`[SGI-S FATAL]` never
  printed**, so the SP was valid every time. The fault inside the handler is some other
  access. (The four clean amplifier runs that preceded this were simply the flake not
  firing; 0-of-4 was never evidence, as noted at the time.)
- **The guard is therefore hardening, not the fix.** It is worth keeping — it closes a real
  unrecoverable path and adds attribution — but it does not close F8, and F8 must not be
  reported as fixed.
- **Why an SP goes bad in the first place.** That is the actual defect; the guard only makes
  it announce itself. Note several boot tests deliberately fabricate bare thread slots into
  `READY`/`WAITING` (`src/process_tests.rs:10489`), which is a known source of contexts with
  no valid stack.

### Next step

Run the harness until it catches again and read `ELR_EL1`/`ESR_EL1`/`FAR_EL1`. If ELR lands
on the tripwire read or the restore `ldp`, the inference is confirmed and the guard is the
fix; if it lands elsewhere, the fault is a different access inside the handler and the
guard is merely hardening that happened to be worth having.

## 10.1 The registers (caught 2026-08-14, `scripts/f8_wedge_repro.py`, F1 amplifier applied)

The harness caught a wedge during `cowstale` and `register read --all` delivered what the
first capture lacked:

```
pc        = 0x401c1200   exception_vector_table + 0x200   (8 of 8 samples)
ELR_EL1   = 0x401c1200   exception_vector_table + 0x200   ← the vector entry ITSELF
FAR_EL1   = 0x401c1200                                    ← faulting on its own fetch
ESR_EL1   = 0x86000004   EC=0x21 instruction abort (same EL), translation fault level 0
TTBR0_EL1 = 0x0041_0000_605dd000                          ← ASID 0x41, L0 base 0x605dd000
TTBR1_EL1 = 0x4045b000   boot_page_tables                 ← kernel high half intact
x30       = sgi_scheduler_handler_with_sp + 424           ← the installer
```

And the wedged run's boot log ends:

```
[AS-NEW]  pid=132 l0=0x605dd000 asid=0x41 via=fork parent=121
[PROC-EXIT] pid=132 ... code=0
[KTG] my_pid=132 my_tgid=132 by_tid=9 code=0 ...
[TERM] tid=9 pid=Some(132) by_tid=14 state=1 ...      ← state 1 = READY at reap time
[AS-FREE] l0=0x605dd000 asid=0x41 path=owner core=0   ← the L0 in TTBR0_EL1, freed
<silence>
```

The live `TTBR0_EL1` **is the L0 the last log line freed**, ASID and all. Kernel text
lives in the TTBR0 low half, so once the freed (PMM-quarantine-poisoned) L0 is installed,
no kernel instruction can be fetched — including the vector entry, which is why the
recursion pins `PC = ELR = FAR = vector+0x200` with a level-0 translation fault. The
earlier §10 symptom table follows verbatim. `x30` names the installer: the scheduler SGI's
context restore (`msr ttbr0_el1` from `(*new_ctx).ttbr0`).

## 10.2 The mechanism

The page-table-UAF free gate (`mmu::any_core_on_l0`, `ACTIVE_L0`/`PREV_L0`) checks only
TTBR0 values **live on cores**. But the scheduler installs `ctx.ttbr0` **verbatim from
saved thread contexts** — and a saved context is a reference the gate cannot see:

1. A process's thread is preempted inside its exit path *before* `deactivate()` — or is
   killed externally and never runs an exit path at all. Its saved `ctx.ttbr0` is the
   dying address space's L0, and it parks READY (the `[TERM] ... state=1` above).
2. The parent's `wait4` reap runs (`publish_child_exit` fires long before `deactivate()`
   in both exit routes), retires the process; reclaim drops the `Process`, and the gate
   sees no core on the L0 — the single core is running the reaper — so it frees and
   poisons the frames.
3. The zombie thread is resumed (a `[REVIVE]`-class transition: the reaper's TERMINATED
   is overwritten by the unconditional WAITING publication in
   `publish_waiting_and_take_pending_wake`, and a waker's WAITING→READY CAS completes the
   resurrection that `mark_thread_ready`/`commit_switch` refuse) — or any other slot whose
   saved context still names the L0 is switched in. The SGI installs the freed L0. Wedge.

Instrumented sweeps (`held_by_ctx` defers, clean suite runs) measure ~30–50 address-space
frees per suite where some slot's saved context still holds the dying L0 at free time —
including several per run in state READY. Every one of those was a loaded gun on the old
code; which run wedged was purely which one got switched in before its context was
re-saved or zeroed.

Why SMP=1 only: at SMP>1 the dying thread is frequently *running* its teardown on another
core at reap time, so its live TTBR0 is on that core and the per-core gate defers the
free. At SMP=1 the reaper running means the zombie is off-core by definition — the gate
passes exactly when the saved-context reference is invisible.

Why F1 amplifies: moving the CoW copy inside `with_address_space` lengthens the exit-path
window between `publish_child_exit` and `deactivate()` under `cowstale`'s fork storm,
making "preempted mid-exit with the dying L0 still saved" far more likely (~3 in 4 suites
versus ~1 in 7).

## 10.3 The fix, tripwires, and verification

**Fix** — the free gate now has a second arm: `threading::any_saved_ctx_on_l0` scans every
slot's saved `ctx.ttbr0`, and `mmu::free_or_defer_as_frames` parks the frames if ANY slot
still references the dying L0 (`[AS-FREE-DEFER] ... held_by_ctx tid=N state=S`).
`drain_pending_ttbr_frees` re-checks BOTH arms before releasing. Every state blocks, on
purpose: FREE/INITIALIZING contexts are overwritten by spawn before the slot can go READY,
TERMINATED contexts are zeroed by the recycler, and live threads re-save at their next
switch-out — so a parked entry always drains, while trusting the state machine costs the
machine if it has a revival route the model missed (it does — see `[REVIVE]`).

**Tripwires kept** (all `safe_print!`, bounded, lock-free):
- `[SGI-S FREED-L0]` — the SGI checks the incoming `ctx.ttbr0` against a 16-entry ring of
  recently freed L0 bases (`mmu::l0_recently_freed`; entries cleared when the frame is
  re-issued as a new L0). This is the wedge one instruction before it happens; it printing
  means the gate has a hole.
- `[REVIVE]` — `mark_thread_waiting`/`mark_thread_running` report when they overwrite a
  cross-thread TERMINATED (the resurrection route measured in §10.2 step 3; left in place
  because abandoning a mid-teardown thread strands whatever it holds).
- `[SGI-S PICKED-TERMINATED]` — `commit_switch` reports a TERMINATED landing between the
  pick scan and the commit.
- The tightened `new_sp` guard from the original §10 stays (real unrecoverable path).

**Boot self-test** — `test_as_drop_defers_while_saved_ctx_on_l0` (src/process_tests.rs),
sibling of `test_as_drop_defers_while_core_on_l0`: plants a dying L0 in a parked slot's
saved context via `threading::test_swap_saved_ctx_ttbr0`, asserts the drop parks the
frames, that a drain refuses while the reference stands, and that clearing it releases
everything with PMM conserved.

**The revival itself is deliberately NOT closed here.** A reaped-mid-exit thread must
finish its teardown (it may hold the lifecycle guard and other locks); the measured trace
shows revived threads complete their exit tail and self-park. With the saved-context gate,
a revived thread's TTBR0 install targets a deferred-not-freed L0 — intact tables — so the
revival is safe from the page-table side. Closing it for real (refusing the WAITING
publication over TERMINATED) changes exit semantics and needs its own verification
campaign; the `[REVIVE]` tracer is there to size the problem first.

## Background

- [`TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md`](TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md)
  — §5.6 is the CoW refcount underflow case study that motivated reading these
  paths together; §8 item 5 is the entry this audit corrects
- [`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
  — §13.9 is the `released_last_va` gate's derivation; the open premature-free
  defect F1 would feed
- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) §5.1 — the `Pte`/`PageTable` newtype, the
  other lever on this code
- [`BKL_PROCESS_CARVE_OUT.md`](BKL_PROCESS_CARVE_OUT.md) §9 — why
  `cow_share_and_demote_range` runs BKL-dropped and what its chunked `as_lock`
  discipline is for
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
  — the no-regression gate for anything in §8
