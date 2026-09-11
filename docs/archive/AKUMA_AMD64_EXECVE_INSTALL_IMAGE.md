# amd64 `execve`: the load/install seam, and three POSIX obligations it buys

**Date:** 2026-09-11
**Status:** landed, verified on QEMU (local) — see §5 for what that covers.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row.
**Follows:** `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE7.md`, whose §8 named
`execve`'s `replace_image` as the next item.

`Process::replace_image_from` was one function doing two jobs. It is now
**load** + `kill_exec_siblings` + **`Process::install_image`**, and
`amd64::usermode::sys_execve` calls the second half.

## 1. Why the seam is between load and install, not at `replace_image`

The obvious fold — amd64 calls `replace_image` — does not work, and the reason
is not the `ProcessInfo` page:

**`replace_image_from` begins by loading the ELF its own way.** It calls
`ImageSource::load` → `akuma-elf`'s `load_elf_with_stack`, which may **defer
segments** for demand paging and builds the stack itself. amd64 loads through
`amd64::loader::load` (the same `akuma_elf::load_elf` underneath) plus its own
`loader::build_stack`, mapping every segment **eagerly**, and C1 step 6 kept
that stack builder deliberately.

Folding at `replace_image` would therefore make amd64 adopt a demand-paging
model it does not have — `lazy_regions` are never read on that target
(`mm::fault_in` resolves through `mmap_regions`), so the shared path's lazy
pushes would be inert while amd64's own eager mapping stopped happening. That is
a capability change wearing a refactor's clothes, which is the thing this series
keeps refusing to do by accident.

Everything *below* the load is the same job on both kernels. So the seam goes
there, and `ImageInstall` carries the result across it:

```rust
pub struct ImageInstall<'a> {
    pub address_space: mmu::UserAddressSpace,
    pub entry_point: usize,
    pub sp: usize,
    pub brk: usize,
    pub stack_bottom: usize,
    pub stack_top: usize,
    pub mmap_floor: usize,
    pub args: &'a [String],
    pub deferred_segments: &'a [DeferredLazySegment],
}
```

amd64 passes an empty `deferred_segments` slice, which says "this target maps
eagerly" in the one place a reader would ask.

## 2. What amd64 gains — three POSIX obligations it never performed

None of these is cosmetic, and none was reachable by the boot suite:

- **`clear_child_tid` is reset.** A `CLONE_CHILD_CLEARTID` address from the
  *previous* image would otherwise be zeroed-and-woken at this process's exit —
  a write into whatever the new program happens to have at that VA, plus a futex
  wake on it. This is live on amd64 since the `clone` fold gave that target real
  `CLONE_CHILD_CLEARTID` handling.
- **Custom signal handlers go back to `SIG_DFL`,** `SIG_IGN` preserved, which is
  what `execve(2)` specifies. A handler address from the old image points into a
  program that no longer exists. Inert today (`rt_sigaction` is a stub on this
  target) and correct the moment it is not.
- **The alternate signal stack is disabled** — it pointed into the old address
  space.

It also picks up `lazy_regions.clear()` and the `[AS-EXEC]` lifecycle trace,
neither of which changes behaviour there.

## 3. Two things that had to be got right

Both were found by reading, not by a failing test — which is worth recording,
because neither would have failed the boot suite.

### 3.1 The `ProcessInfo` page, again

`install_image` re-maps one into the new address space. The page is per-*address
space*, so a fresh one is mandatory here rather than merely tidy: the old one
went with the space the function just dropped.

amd64 has no such page. Slice 6 had already built the hook for `fork`
(`fork_alloc_process_info`); it now serves both and is named
`ExecRuntime::alloc_process_info`. amd64 returns `0`, and the `write_phys` is
skipped on the same value — one hook rather than two, because the map and the
write gate on the same answer.

### 3.2 The target `Process` is the thread-group leader's

The block this replaced resolved through `with_process(current_pid(), …)`. Since
the `clone` fold, `current_pid()` is the **tgid** — so that code installed onto
the *leader's* `Process`.

My first version used `current_process()`, which is the *own* half. For a main
thread the two coincide; for a non-leader `execve` it would have installed the
new image onto the calling thread's own `Process` and left the group leader
still running the program that was just replaced. It is
`current_thread_tgid_process()` now.

`execve` replaces an address space, which is a property of the thread *group*,
so the leader is right on both counts — the same reasoning `current_mm_process`
carries for the fault path (slice 7 §2.4). **This is the third time in two
slices that the own/tgid distinction has mattered**; assume it matters.

### 3.3 Ordering: who puts the new space in the MMU

`install_image` calls `UserAddressSpace::deactivate()` before the swap, so the
hardware is off the old tables before they are freed — which is what makes
dropping an address space this core was running on safe at all. It deliberately
does **not** install the new one: on AArch64 the `eret` path does that, and on
amd64 `sched::set_current_space_root` does.

So `sys_execve` still owns that line, and it must run before ring 3 is
re-entered — which `run_process`'s loop does immediately after this syscall
returns. Between the `deactivate` and that re-install there is **no user memory
access**: the `ProcessInfo` write goes through the physmap and the page tables
are reached the same way.

amd64's two explicit `drop(old_space)` / `drop(old_regions)` calls are gone: the
install drops both, after the `deactivate`, which is the ordering those two
lines were arranging by hand. The reason they were *outside* the old hold —
`UserAddressSpace::drop` frees every user frame and page table, not work for a
closure `with_process` runs with interrupts disabled — does not apply to
`install_image`, which takes `&self` and interior locks at normal priority.

## 4. The AArch64 binary is the proof this is an extraction

`.text` **−776 bytes**, and **only the two symbols involved in the split
changed**:

| symbol | before | after |
|---|---|---|
| `replace_image_from` | 5132 | **788** (load + kill_siblings + a call) |
| `install_image` | — | **3568** (new) |

`5132 − (788 + 3568) = 776`, which is exactly the `.text` delta — the shrink is
the `ProcessInfo` block becoming a hook call. Nothing else in the binary moved.

## 5. Verification

| gate | result |
|---|---|
| AArch64 Lima/KVM boot A/B (`SMP=1`) | 306 occurrences, **298 distinct, identical set, 0 failed** both arms |
| AArch64 `.text` | **−776 B**, accounted for exactly (§4) |
| QEMU/TCG `SMP=4` | **641/0** (×3), `execve:` block green |
| QEMU/TCG `SMP=1` | **631/0** |
| `amd64_ring3_check --smp 1 -n 60` | **RING-3 CHECK: OK** |
| host tests | **1375** |
| clippy — aarch64 `release` + `extreme-size`, amd64 | clean |

The ring-3 check is the gate that matters: the boot suite's `execve:` block runs
as init, while `grandfork` **step 4 is a grandchild that `exec`s** with its
parent waiting — the folded path reached from ring 3, through two levels of
fork. It reported `ALL PASS`, with 60/60 sessions, `free` unmoved, `ps` 5 → 5
and a heap drift of +68 KiB against an 8192 KiB tolerance.

## 6. What is left of `sys_execve`

The `execve` **re-entry loop** stays, and it is not a leftover: `exec_pending`
→ `run_process` goes round again is this target's *only* mechanism for `execve`,
and it exists because `enter_ring3` returns on x86_64 where `eret` does not on
AArch64. Folding it away is the lifecycle unification
(`proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` §4 option 2), which is now the
one structural item left on that prompt.

Also still amd64's: the path/argv/envp read, the BKL-free image read
(`exec_runtime::bkl_free_io`), `thread::drain` with its stated non-leader
carve-out, `clear_group_exiting`, and the display `name` — which is the
*resolved path*, not `argv[0]`, because `/proc/<pid>/exe` reports it.

## Background

- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE7.md` — the `clone` fold, and §2.4's
  own/tgid distinction that §3.2 here is the third instance of.
- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE6.md` §3 — the `ProcessInfo` hook
  this reuses.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` § C1 step 6 — why amd64 kept
  `build_stack`, which is why the seam is where it is.
