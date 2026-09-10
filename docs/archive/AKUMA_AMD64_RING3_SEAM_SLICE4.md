# amd64 ring-3 entry seam, slice 4: the fork memory pass gets an architecture seam

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row.
**Slice:** 4 — the step slice 3's §1 re-scoped this to be.

`fork_process`'s step 4 — "build the child's address space from the parent's" —
is now a registered hook. AArch64 registers it verbatim; amd64 registers a walk
that speaks x86 page tables. **This is the last structural thing between the
amd64 kernel and calling the shared `fork_process`.**

## 1. What was wrong, restated with the measurement

`fork_process` compiles for `x86_64-unknown-none` (slice 1) and its memory pass
would have been **silently wrong**. Every page it touches goes through an
`akuma-mmu` family that takes a raw `*const u64` root — `translate_user_va`,
`collect_mapped_pages_with_flags_into`, `for_each_mapped_user_pte`,
`demote_range_to_ro` — and walks it with AArch64 descriptor semantics:
`flags::VALID`, `flags::TABLE`, the ARM block/table distinction, and
`AP_RO_ALL`/`UXN`/`PXN` for permissions. None of it is `#[cfg]`-gated.

The bits **near-miss**, which is what makes it silent rather than loud: ARM's
`VALID` is bit 0 and so is x86's `Present`; ARM's `TABLE` is bit 1, where x86
has `R/W`. A walk handed a PML4 descends or stops according to whether the
parent's pages happen to be writable, and builds a child address space that is
garbage without any call returning an error.

**A hook, not a `cfg`.** The other kernel does not want a variant of this
function — it has a correct implementation of the same job already, over
`UserAddressSpace::rewrite_leaves_in_range`, the leaf iterator that *does* have
both arms. Merging the two into one function pretending to be portable would
throw that away. This is the same argument §4 of the prompt made for
`ExecRuntime::enter_user`, and it wins here for the same reason.

## 2. What was done

### 2.1 `ExecRuntime::fork_share_memory`

```rust
pub fork_share_memory: fn(
    parent: &Process,
    child: &mut Process,
) -> Result<(), &'static str>,
```

`fork_process`'s step 4 became one call. The child is unpublished and in the
caller's exclusive hands, so the implementation may take `get_mut()` on its
address space; the parent is shared, so every access to *its* page tables goes
through its own lock.

### 2.2 AArch64 registers the pass verbatim

493 lines lifted out of `fork_process` into
`akuma_exec::process::fork_share_parent_memory`, with **no logic change**. It
needed only `parent` and `child` from the enclosing scope — `parent_pid` and
`parent_tgid` are read off `parent` rather than passed, so a caller cannot
disagree with the `Process` it just handed over. The tgid rationale (lazy
regions are tgid-keyed, and a worker-thread fork that enumerated by pid drops
every sibling's stack) moved to sit with the binding that uses it.

`akuma-kernel-glue` registers it. The AArch64 kernel therefore calls the same
code through one indirection.

### 2.3 amd64 registers its own walk — which already existed

`Image::fork_of`'s share pass was extracted to
`share_parent_memory_into(parent, child_space, child_regions)` and now has two
callers: `fork_of` (unchanged behaviour) and the new
`usermode::fork_share_memory`, which points it at the child `fork_process`
already built rather than at a fresh address space.

That matters for verification: the hook is not new code wired up and untested.
It is a thin wrapper over the walk every `fork` on this target has always taken,
which the boot suite's `fork:` checks exercise on every boot.

**The `ProcessInfo` page is not the hook's problem.** `fork_process` maps one
into the child before calling and re-maps it after, because the walk covers
`PROCESS_INFO_ADDR` and would otherwise leave the child sharing the parent's
copy. The AArch64 side carries the same ordering for the same reason.

## 3. Verification

### 3.1 The AArch64 kernel: the lift is behaviour-preserving

This moved 493 lines of the fork hot path, so a binary comparison is not
enough and the boot A/B was run.

**Binary.** `.text` **+2532 bytes**, and the symbol-level accounting is exact:

| symbol | before | after |
|---|---|---|
| `akuma_exec::process::fork_process` | 5212 | **2128** (−3084) |
| `akuma_exec::process::fork_share_parent_memory` | — | **7804** (new) |
| `akuma_kernel_glue::kernel_main` | 55632 | **55644** (+12) |

Those are the only symbols whose size changed. The outlined function is bigger
than the inline block it replaced because it can no longer share code with its
caller; that is the price of the seam, stated rather than hidden.

**Boot.** `scripts/lima_aarch64_run.sh` (KVM inside Lima, `SMP=1`), committed
tree and the change side by side, each run until its test set settled:

| | before | after |
|---|---|---|
| distinct `[Test] … PASSED` | 283 | **283**, identical set (`diff` empty) |
| `PASSED` occurrences | 306 | **306** |
| failures | 1 | **1**, the same pre-existing `test_mmap_file_oom_survives` |

### 3.2 amd64

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=1` / `SMP=4` | 631/0 · 641/0 | **631/0 · 641/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 609/0 · 619/0 | **609/0 · 619/0** |
| bare metal `SMP=4` | 634 + 3 xHCI | **634 + 3 xHCI** — the same three |
| `ring3_check --smp 1 -n 60` | — | **60/60**, `free` unmoved, `ps` 5 → 5, `grandfork` ALL PASS |
| host tests | 1373 | **1373** |
| clippy — aarch64 `release` + `extreme-size`, amd64 ±`no-tests` | clean | **clean** |

Zero delta on every amd64 rig, which is what a seam whose amd64 arm nothing
calls yet should show.

## 4. What is left before `sys_fork` can call `fork_process`

Two stubs, both **loud** (they fail the syscall rather than corrupting a child),
and neither is architecture-hard the way the memory pass was:

- **`akuma_threading::get_saved_user_context`** — x86 arm returns `None`, so
  `fork_process` fails at step 6. It is the read mirror of the
  `update_thread_context` writer slice 2 built: an
  `X86ArchHooks::read_user_context` over `machines()[slot].uctx`, plus the
  `user_rip == 0` refusal `sys_fork` already performs by hand.
- **`ThreadPool::spawn_user_closure_initializing`** — x86 arm returns `Err`.
  The harder of the two: an amd64 process task carries a `space_root` and a
  `proc_slot` that shared code has no concept of, so the arm has to claim the
  slot and stacks while something else supplies those two — most naturally
  `spawn_child_thread_and_publish`'s `before_ready` closure, which runs in the
  last window where the child provably has not executed an instruction.

After those, the fold itself, and then `clone` as a separate step.

## Background

- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE3.md` §1 — where this slice's scope
  came from, and the measurement that found the walker.
- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE2.md` — `ExecRuntime::enter_user`,
  the seam this one is modelled on.
- `docs/archive/AKUMA_AMD64_COW.md` — amd64's own CoW fork, i.e. the walk §2.3
  reuses.
