# amd64 C1 step 5a: one x86 user-space walker

**Date:** 2026-09-08
**Status:** landed. Four rigs green, ring-3 verified on the metal.
**Parent:** `proposals/AMD64_STEP5_PROCESS_TABLE.md` §5a — the first half of C1
step 5, and the thing the plan chart draws as a sibling when it is a prerequisite.
**Hand-off:** `proposals/NEXT_AGENT_AMD64_STEP5_PROCESS_TABLE.md`, which now
starts at step 6 / 5b.

---

## What this step did

`amd64/src/paging.rs` served two roles and only one of them was duplicated:

1. the **kernel's own** mappings — `map_page`, `active_root`, `activate`,
   `drop_identity_map`, the MMIO windows `pci.rs`/`lapic.rs`/`blk.rs` map;
2. the **user address space** — `AddressSpace::{new,root,map,translate,prot,free}`,
   `map_page_in`, `unmap_page_in`, `translate_in`, `prot_in`,
   `for_each_user_leaf`, `for_each_leaf_in_range`.

Role 2 was a second, complete x86 page-table walker beside
`akuma_mmu::UserAddressSpace` — the one `akuma-exec`, `akuma-elf` and
`akuma-syscalls-glue` reach through, and the one item C1 folds this kernel onto.
Role 2 is gone. `mm.rs`, `idt.rs`, `loader.rs`, `usermode.rs` and `fd.rs` all map
through the crate now; role 1 stays exactly where it was, because `akuma-mmu`
does not do kernel mappings on either architecture.

`Process` lost two fields and gained one:

```
- space: paging::AddressSpace,   // a bare u64 root, no lock, reached through
-                                // `static mut PROCS` + a raw pointer
- frames: FrameSet,              // a second frame ledger beside the page table
+ space: ProcAddressSpace,       // Spinlock<UserAddressSpace> + a lock-free
+                                // atomic mirror of the root for the fault path
```

which is the shape `akuma_exec::Process` requires — `ProcAddressSpace::new` takes
a `UserAddressSpace` and nothing else, so **5b could not have been written first**.

## Tallies

Every rig at `SMP=4`. 13 new boot checks; every arm is baseline + 13, zero
failures.

| rig | before | after |
|---|---|---|
| QEMU/TCG | 495 / 0 | **508 / 0** |
| Firecracker (the box, KVM) | 484 / 0 | **497 / 0** |
| OVMF/GRUB (the box, multiboot2 under KVM) | 488 / 0 | **501 / 0** |
| bare metal | 488 / 0 | **501 / 0** |

Also held: host tests **1360** passing / 143 suites; the ten `c_stress` memory
probes at **8/10, 0 unexpected failures** (`scripts/utils/amd64_mem_trials.py`);
clippy clean on the amd64 kernel, the AArch64 kernel and `akuma-syscalls-glue`
for `x86_64-unknown-none`.

**The AArch64 kernel is byte-identical**: `.text` 3 207 052, `.rodata` 270 368,
`.data` 199 896, all three unchanged against `HEAD`. Everything added to
`akuma-mmu` is behind `cfg(target_arch = "x86_64")` or `cfg(any(x86_64, test))`.
No `.rodata` line-number drift this time — the comments added above shared code
did not move a panic `Location`.

### The ring-3 check on the metal

The boot suite runs under `BypassValidationGuard` and cannot prove a user-facing
path, so this is separate. On the bare-metal box, over ssh (which is itself a
userspace ELF that forks per session):

- 20 shell pipelines (`echo | cat`), then 60 `/bin/true` spawns, `find /`, and a
  76 102-line `grep` — all correct.
- `free` used: 1 053 433 KiB before, **1 053 169 KiB after** ~85 process
  lifetimes. The new `Drop` teardown returns everything on real hardware; the
  old `Process::free` path could not have been swapped out on a boot tally alone.
- `/proc/self/maps` renders three correct rows — that is the `fd.rs` path this
  step re-pointed at `with_current_address_space`.
- `ps` lists the session's own process tree.

## What moved into `akuma-mmu`, and why each is x86-only

### The PTE-level entry points

`UserAddressSpace::map_page` takes an **AArch64** flag word, because that is what
`akuma-exec`/`akuma-elf` hold, and decodes it down to a neutral
`akuma_mmap::Prot`. Two things are lost on the way and both are load-bearing here:

- **The CoW marker.** `Prot` is a *region's* protection and has no business
  carrying a page-table software bit, so nothing that goes through `user_flags`
  can set x86 PTE bit 9 — and `fork`'s demote, `mprotect` over a shared frame and
  `mremap`'s re-point all have to.
- **`RO`/`RX` collapse.** A caller that already knows the exact triple it wants
  should not have to find a `Prot` tag that decodes back to it.

So: `map_page_pte(va, pa, PteProt, cow)`, `map_and_track_pte(va, frame, PteProt,
cow)` and `pte_prot(va) -> Option<(PteProt, bool)>`. x86-only for the same reason
the range walks are: the AArch64 side has no marker to pass and no caller that
wants one.

`map_and_track_pte` tracks **before** it maps and untracks again if the map fails
— the obligation `loader::map_range` used to discharge by hand with a
`track_user_frame` above the `map`. A frame the ledger does not know about is a
frame teardown will not release; a frame it knows about that nothing maps is one
this address space would free out from under its next owner.

### `LeafAction::Remap`

`MADV_DONTNEED`'s break-sharing arm has to point a leaf at a **different** frame,
and it has to do it inside the walk: `rewrite_leaves_in_range` holds `&mut` on the
address space, so a `map_page_pte` from inside the closure cannot borrow. `Remap`
is one store on a leaf whose page tables already exist. Like `Unmap`, it does not
free the old frame or touch its refcount — the caller was handed the `pa`.

### The walk now hands the closure the frame ledger

`rewrite_leaves_in_range(&mut self, start, end, f: FnMut(&FrameLedger, Leaf) -> LeafAction)`.

Not a convenience. **Every** real caller of the mutating walk touches the ledger
in the same step — `munmap` drops one VA's claim on each frame it clears,
`MADV_DONTNEED` swaps one frame for another — and the amd64 spelling of that used
to be `usermode::untrack_anon_frame`, which resolves through `PROCS` and takes the
address-space lock. Called from inside the walk that is holding it, that is a
same-core deadlock. The alternatives were both worse:

- collect the leaves into a `Vec` and make a second pass — a heap allocation sized
  by *residency*, on the one syscall that runs when memory is short, which is the
  exact thing `rewrite_leaves_in_range` was written to avoid;
- make the receiver `&self` — which works (the walk writes through raw pointers
  anyway, as `update_page_flags_inner` already does) but gives up the `&mut self`
  that documents "you hold the per-AS lock".

Passing the ledger keeps `&mut self` *and* states the boundary: the closure may
edit the ledger and its own leaf, and nothing else.

### `impl Drop for UserAddressSpace` (x86)

The comment that stood where this now is said freeing an address space still
installed in `CR3` unmaps the code doing the freeing, so the free had to be asked
for. **That danger has not gone away; what changed is that the gate against it is
fed.** `paging::activate` brackets its `mov cr3` with
`publish_l0_begin`/`publish_l0_end` (landed as this step's prerequisite), so
`any_core_on_l0` answers truthfully, and the destructor routes everything through
`free_or_defer_as_frames` — the one exit both architectures release page-table
frames through, which parks the frames when that gate or the saved-context gate
says the table is still referenced.

Three pinned divergences from the AArch64 twin, all of things this target lacks:

- **No ASID.** `asid()` is a pinned `0`, so there is no `flush_tlb_asid` and no
  allocator to return a tag to; every `CR3` write is a full non-global flush.
- **No `SHARED_L0_TABLE` arbitration.** `new_shared` registers nothing, so a
  shared view frees nothing at all here rather than decrementing a refcount and
  possibly inheriting the owner's deferred frames. That is correct for what this
  target builds today — the only `new_shared` callers are `uas.rs`'s boot checks
  and `idt.rs`'s borrowed fault view — and it is **load-bearing to keep it that
  way**: the day `clone(CLONE_VM)` gives a thread its own shared
  `UserAddressSpace`, an owner dropping first would free page tables a live view
  is still walking, and the registry has to arrive with it.
- **No lifecycle instrumentation.** `instr::as_drop_enter`/`as_drop_exit` are
  `leak-instr`-only accounting for a heap leak this target has never had.

Teardown semantics were checked rather than assumed, and are equivalent:
`loader::free_all_frames` freed a user frame only when `cow_ref_dec` reported the
last reference, and `free_as_frames_now` calls `akuma_pmm::free_page_at`, whose
first line is that same gate. The crate's version adds untrack-before-free
ordering, the premature-free check and (where enabled — amd64 sets
`pmm_uaf_quarantine: false`) the UAF quarantine. Page-table frames differ only in
*how they are found*: amd64 re-walked the tables, the crate frees the ledger's
tracked set, which is the leak B3 closed by making the ledger mandatory.

### One `PteProt`, one encoder

`amd64/src/paging.rs` defined its own `PteProt`, its own `MemAttr` and its own
`encode`, structurally identical to `akuma-mmu`'s and held in step with them by a
host test (`x86_prot_matches_amd64_encoding`) that compared the two arm by arm.
Two implementations, one agreement. There is one implementation now:
`akuma_mmu::encode_pte(prot, attr, cow)` is `pub`, `paging.rs` re-exports the
types and forwards to it, and its boot self-test `region_prot_roundtrip_check`
pins that single encoder against **literals** on real hardware.

`cow` is `encode_pte`'s third argument rather than a field of `PteProt`, because
it is not a permission — it is a software marker the hardware ignores, and every
mapping `paging.rs` makes is the kernel's own, which never carries it. The
demotion that used to be hidden in a `PteProt::cow()` constructor is now the
caller's and visible: `USER_RW` demoted *is* `USER_RO`, spelled out.

## Findings

### 1. `cow_write_fault` had to keep reading `CR3`, and that is the right answer

The obvious port was to resolve the faulting address space through the running
process — `with_current_address_space`. It compiles, it is more "correct-looking",
and it silently deletes the only coverage the copy-on-write break has that does
not need a live user program: `uaccess.rs`'s `CR0.WP` self-test maps a CoW-marked
pair into the **kernel's own root** and drives the `#PF` path from ring 0. There
is no process, so the accessor answers `None` and the test would have passed while
proving nothing.

`UserAddressSpace::new_shared(active_root())` is the shape that keeps both: a
borrowed view that names an existing L0 and whose ledger owns nothing, so it frees
nothing when it drops. It is also the more honest reading — `CR3` says *which
address space the fault happened in* directly, rather than deriving it and hoping
the derivation agrees, which is the same discipline `translate`/`prot` follow by
walking rather than consulting a shadow record. The two answers coincide whenever
there is a process: this target's scheduler installs a user task's root in kernel
mode as well as ring 3.

Nothing on that path allocates a page table — every arm rewrites a leaf that is
already present — so the view's throwaway ledger never has anything to lose. The
*real* ledger update stays `usermode::cow_swap_frame`, against the running
process, which is where a replaced frame has to be recorded.

`uaccess.rs`'s own test was moved onto the same borrowed view, so the pages it
installs and the walk the fault handler does are one implementation rather than
two that could disagree about what a demoted page looks like.

### 2. The boot suite deliberately leaked 12 KiB per boot, and said so

`uas.rs`'s smoke test ended with
`t.note("uas: page-table frames deliberately leaked (no Drop on this target)", 3)`.
That note is the reason the `Drop` impl is testable at all: it named the exact
property that had to change, so "does `Drop` return the frames" had a
before-and-after rather than being a new claim with no baseline.
`drop_returns_frames_test` now pins it as a PMM free-count round trip — the one
observation that cannot be satisfied by the ledger agreeing with itself — and the
frame count as an equality (`2 data + 4 tables + 1 L0 = 7`) rather than a lower
bound, because a walker that started tracking the *shared* PML4 slots' tables (the
kernel's own) would show up here as a larger number before it showed up as a dead
machine.

The first run of that check failed at `got 7 want 8`: two VAs 2 MiB apart are two
*entries* of one PD, not two PDs. Worth recording because the arithmetic is the
check.

### 3. `have_address_space()` stopped being the thing that prevents the bug

`mm.rs` guarded five entry points with `have_address_space()` before touching
`paging::active_root()`, because that function answers with `CR3` whoever asks: on
a kernel thread it is the kernel's own root, and a `munmap` walking a user range in
it is at best a no-op and at worst an unmap of something the kernel put there. The
guard had to be *remembered* at each site.

There is no root to pass now — `with_current_address_space` answers `None` — so the
class is closed by construction. The checks stay, and the comment at each says why:
the **errno** is the point. A caller with no process must see `ESRCH`, not the `0` a
silently-skipped unmap would return.

### 4. What a shared-crate insertion looks like when the anchor text is duplicated

The PTE-level entry points landed in the **AArch64** `impl` on the first try: the
anchor (`map_and_track`'s body) is byte-identical in both arch blocks, and the
scripted edit took the first match. It failed loudly at compile time — `PteProt`
is `cfg`-ed out on that side — which is the good case. Worth stating anyway
because the *silent* version of that mistake is the one this tree keeps paying
for: an insertion that compiles in both blocks and only runs in one.

## What this step deliberately did not do

- **Step 6 (`loader.rs` → `akuma-elf`).** The dependency reason for the two
  loaders has expired — every signature in `loader.rs` already takes
  `&mut UserAddressSpace`, `EM_NATIVE` is `cfg`-selected, and
  `impl UserPages for UserAddressSpace` is arch-neutral. What is left is a **VA
  layout**: this file's `PIE_BASE` (`0x1000_0000`), `INTERP_BASE`
  (`0x4000_0000`) and `ELF_STACK_TOP` were chosen against `mm::MMAP_BASE`, and
  `akuma-elf` picks different ones. Moving to it moves where every program on
  this target lands, which wants its own A/B.
  - **Found on the way:** `akuma_elf::interp` compares `e_machine` against a
    hardcoded `EM_AARCH64` where `load.rs` uses the `cfg`-selected `EM_NATIVE`.
    Nothing has noticed because no x86 caller has reached the interpreter path
    yet; it will refuse every dynamic binary the moment one does.
- **The `smp-shared` / IRQ-masking question.** amd64 depends on `akuma-exec`
  *without* that feature, so `ProcAddressSpace::lock()` does **not** mask IRQs
  while this target runs `SMP=4` on its own ticket BKL. 5a's holds are one PTE
  edit or one bounded walk and nothing that blocks, which is why it is survivable
  here; **5b puts a shared process table behind that lock and must decide it.**
  Note `akuma_cpu::daif` is a silent no-op on x86_64, so `IrqGuard` does nothing
  on this target either — the `IrqGuard`s already scattered through `mm.rs` and
  `akuma-user-space` are decorative here, and giving `daif` a real `cli`/`sti`
  arm is a change with reach far beyond this step.
- **Region-driven vs table-driven `mm.rs`.** The two kernels genuinely differ and
  5a kept the amd64 shape. Unifying is C2's call, with the syscall fold in front
  of it.

## The lock order this step established

**regions → address space → PMM**, in that direction only.

`fault_in` and `dontneed_range` are the two that hold both of the first two;
`sys_mmap` takes the region lock to reserve and releases it before populating.
Nothing takes them the other way round, and nothing may start — the deadlock that
shape prevents is not hypothetical, it is what `untrack_anon_frame` inside
`rewrite_leaves_in_range` would have been.

`populate_file_page` reads the file **outside** the address-space hold on purpose:
that read takes the descriptor table, and taking it underneath this lock would be
the only place in the module where those two are ordered that way.

## Background

- `proposals/AMD64_STEP5_PROCESS_TABLE.md` — the plan, and the two prerequisites
  (the x86 range walks, and feeding the live-L0 registry) with their findings.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` § C1 — the unlock tree.
- `docs/archive/AKUMA_AMD64_B3_ADDRESS_SPACE.md` — the x86 `UserAddressSpace`
  this step finally put to work.
- `docs/archive/AKUMA_AMD64_COW.md` — the copy-on-write break this step re-pointed.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — why `pte_prot_for` reads the
  share count before granting a write.
- `docs/runbooks/amd64-bare-metal-loop.md` — the four rigs.
