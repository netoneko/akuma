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
**CONFIRMED (mechanism) / NEEDS-REPRO (consequence).**

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

### F2 — `try_resolve_el1_cow_fault` resolves the wrong process
**CONFIRMED (mechanism) / NEEDS-REPRO (consequence).**

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
| 3 | **`spawn_child_thread_and_publish(...)`** — the ~40-line tail of §2, shared by all three primitives with `clone_thread`'s two documented opt-outs as parameters | −80 | medium | Highest structural payoff in the pile. One place for the ttbr0 fix, the `THREAD_PID_MAP` ordering, and the signal-mask seed that must precede `mark_thread_ready` |
| 4 | **`Process::inherit_from(parent, overrides)`** — one constructor, 36 inherited fields in one place, 9 explicit | −120 | medium | Kills the four-45-field-literal problem. Do it *after* 3 so the tail is already shared. Must not become a `Default` — every one of the 9 must stay a compile error if unset |
| 5 | **F2's owner resolution** (`read_current_pid` → `address_space_owner_pid_for_fault`) | 1 line | medium | A one-line fix to a two-frame-per-fault leak. Needs the boot suite at SMP=4 and a PMM drift check, not a merge |
| 6 | **F1's copy-under-lock** — move the 4 KiB copy inside `with_address_space` in both EL1 paths | ~10 | **high** | Extends a lock hold to cover a 4 KiB copy on the kernel-write path. Correct per the EL0 path's own invariant, but it is a hold-time change in the fault path and needs measurement |
| 7 | **The DA/IA demand-paging bodies** (§6) | −330 | **high** | Biggest number here and the most tempting. Do not do it as hygiene: answer the `is_exec` and barrier-placement questions first, on purpose, with SMP=4 evidence |

Items 1 and 2 are an afternoon and are pure clarification. Items 3 and 4 are the
ones that change how this code ages — after them, a new `Process` field is one
edit and the child-publish ordering has one definition. Items 5–7 are defect work
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
>   for `PMM_EXTRACT.md` to remove with the other 13.
> - **Two in-code comments cross-referenced `try_break_cow_for_kernel_write`, a
>   function that does not exist** in this tree under that name — the EL1 pre-flight
>   is `ensure_cow_page_writable`. Both cross-references are gone: they now point at
>   the one helper.
>
> Items 3–4 of §8 (`spawn_child_thread_and_publish`, `Process::inherit_from`), item 7
> (the DA/IA merge) and the F1/F2 fixes remain open, as scoped.

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

## 9. Findings summary

| id | finding | status | verify with |
|---|---|---|---|
| F3 | `cow_fault_lock` provides no mutual exclusion; 3 of 4 CoW runtime fn pointers are dead | **CONFIRMED** | `grep -rn COW_FAULT_LOCK src crates` → 3 hits, all in `pmm.rs` |
| F2 | `try_resolve_el1_cow_fault` resolves the owner with `read_current_pid()`; for `CLONE_VM` workers this leaks two frames per kernel-side CoW break and takes a lock nobody waits on | **CONFIRMED** mechanism, **NEEDS-REPRO** consequence | PMM drift across a threaded workload taking kernel-side user writes; `[AS-*]` traces |
| F1 | both EL1 CoW-break paths copy 4 KiB from `old_pa` outside the lock the EL0 path documents as required for `old_pa`'s validity | **CONFIRMED** mechanism, **NEEDS-REPRO** consequence | `cowstale`, `bssfork spread=1` at SMP=4; poison decode in `[WILD-DA]` |
| F4 | DA single-page fallback places `dsb ish; isb` before the PTE install, disagreeing with its own batch path and with the IA arm | **CONFIRMED** | inspection; no known symptom |
| F5 | IA arm claims `icache_done: true` into `file_page_cache` for pages it may map non-exec | **CONFIRMED** | inspection; no known symptom |
| F6 | re-entrant `fault_slot_acquire` on one page by one thread lets the inner release drop the outer's entry | **CONFIRMED** | inspection; no known trigger |
| F7 | survey errors: item 5 is three guards not two, ~24 lines not 142; `log_fault_reclaim`'s rustdoc opens with another function's sentence | **CONFIRMED** | §7 |

None of F1–F6 is being fixed in this document's change. F1, F2 and F4–F6 are
recorded here because they are invisible from any single call site and only
surface when the copies are read side by side — which is the argument for §8's
items 3 and 4.

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
