# amd64: `akuma-mmap` adopted, and demand paging behind it (B1 + B2)

**Date:** 2026-09-07
**Scope:** items **B1** and **B2** of the unlock tree in
`docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — wire the shared region table into
`amd64/src/mm.rs`, then teach the `#PF` handler to service a not-present fault
out of it.
**Status:** done and verified on three rigs.
**Proposal:** `proposals/NEXT_AGENT_AMD64_MMAP.md`.

---

## What was wrong

`amd64/src/mm.rs` was an eager bump allocator with no region table. Every one of
its limits was a wall `rustc` hits, and they were not independent — each was a
consequence of having nowhere to record what a process had mapped:

| | before | now |
|---|---|---|
| largest mapping | `MAX_MAPPING` = 64 MiB, `EINVAL` past it | the VA window; `ENOMEM` past it |
| lazy path | none — `EAGER_MAX_PAGES = usize::MAX` | `akuma_config::MMAP_EAGER_MAX_PAGES` (16), demand-paged past it |
| placement | one **global** bump `NEXT_VA`, shared by every process, never reused | per-address-space first-fit over the region list |
| `MAP_FIXED` | `ENOSYS` | honoured; replaces what it lands on |
| `munmap` | could not clip or split | `akuma_mmap::detach_eager_regions_in_range` |
| `mprotect` | `return 0` — accept and do nothing | splits regions, re-permissions present pages |
| teardown | walked `[MMAP_BASE, NEXT_VA)` because nothing recorded ownership | the frame ledger, like every other page |
| file-backed | `ENOSYS` | still `ENOSYS` — needs a page cache |

## The shape of the change

### `PteProt`, and why the rename was the point

Two types were both called `Prot`. `akuma_mmap::Prot` is the **region**
vocabulary — what a mapping is supposed to be. `amd64::paging::Prot` is the
**page-table** vocabulary — what the hardware is told, including a CoW marker
bit that is not a permission at all. The amd64 one is `PteProt` now, and
`PteProt::from_region` is `akuma-mmap`'s x86 backend.

Two arms of that function are **pinned divergences**, both asserted by
`paging::region_prot_roundtrip_check`:

- `RO` and `RX` collapse to the same PTE. They differ only in `PXN` — whether
  EL1 may fetch — and x86 has one execute bit, not two.
- `RW` (writable *and* executable on AArch64) becomes non-executable here.
  `sys_mmap` refuses `PROT_WRITE | PROT_EXEC` with `EINVAL` and
  `Prot::from_prot` never yields `RW`, so no region on this target can carry it;
  if one ever does, dropping execute produces a visible fault at the fetch where
  granting it would hand ring 3 a writable code page.

The pin is 17 checks: every named constant and every `Prot::ALL` variant against
the literal `u64` it encodes to, plus the `Prot::ALL` **arity**, so a seventh
variant fails the boot suite instead of landing silently on `from_region`'s
fail-closed arm.

### Where the region list lives

`Process::regions` in `usermode.rs`, a `Spinlock<Vec<MmapRegion>>`, reached
through `usermode::with_current_regions`. `PROCS` is keyed by `proc_slot`, and a
`clone(CLONE_VM)` thread runs on its caller's slot, so a region list is
per-address-space — which is what it has to be.

The lock is real, not ceremony: this target is not BKL-serialised at `SMP=4`
across the window `yield_now` opens, and the AArch64 race it mirrors
(`docs/archive/AKUMA_MMAP_REGIONS_RACE.md`) is exactly `CLONE_VM`, which amd64
now supports.

`MmapRegion::frames` is left **empty** and `pages` carries the extent — the
CoW-inherited shape the crate documents. Frame ownership here is
`akuma_user_space::FrameLedger`'s job; a second frame list in the region would
be a second answer to the same question and would drift the first time a CoW
break swapped a frame.

### `sys_mmap` reserves under the lock, populates outside it

The region is pushed first, so a concurrent `mmap` on another core cannot pick
the same range; the frames are allocated after the lock is dropped, so the PMM
is never entered from inside it. `fault_in` makes the opposite trade — it maps
exactly one page and holds the lock across it, which closes the window where a
concurrent `munmap` retires the region between the lookup and the map.

### The range walker

`paging::for_each_leaf_in_range` descends once and skips an absent subtree
whole: a missing PML4 entry advances the cursor 512 GiB, a PDPT entry 1 GiB, a
PD entry 2 MiB. Without it, `munmap` of a 1 GiB lazy reservation would be a
four-level walk per page across 262144 pages of which zero are present — and
`rustc` makes reservations that size.

---

## Two bugs found on the way

Neither was the task; both are the kind that surface far from their cause.

### 1. A `fork` child leaked every page it later `mmap`ed

`Process::free` had two paths. `mmap` did not record its frames in the ledger,
so teardown walked the global bump window and released what was still mapped —
but **only for a process that was not a `fork` child**, because a child's ledger
already held every page the fork shared and the walk would have decremented each
twice.

The hole is a child that calls `mmap` *after* forking. Those frames were in
neither the ledger nor the walk, so they leaked until reboot. A shell is exactly
that shape: fork, then let musl's allocator mmap an arena.

`sys_mmap` and `fault_in` both `track_user_frame` now, so there is one teardown
path for every process and it is the one the loader's pages have always taken.
`release_anon_frames` and the `forked` special case are gone.

### 2. `munmap` could double-free a loader page

The old `sys_munmap` called `cow_ref_dec` + `free_page` on any frame it
unmapped, including one the ledger still held — which `Process::free` would then
release a second time. Frames go back through `untrack_anon_frame` now: a frame
this address space does not track is left alone, because freeing it here would
be a double free against teardown.

---

## The `MAP_SHARED | MAP_ANONYMOUS` gap, found by a probe

`sys_mmap` recorded `shared_anon` on the region from the start, and `fork`
ignored it — so a shared anonymous mapping was CoW-copied like everything else
and the child's writes were invisible to the parent. `shmanon` caught it
immediately, which is the whole argument for running the existing suite rather
than writing a fresh probe.

`fork_from` now reads the parent's shared ranges out of the region list in the
same hold that builds the child's inherited list, and maps any page inside one
**by identity**: same frame, writable in both, no CoW marker, parent's PTE left
alone.

There is a second half to it. A shared anonymous region must never be lazy on
this target, whatever `plan` says: sharing happens by walking the parent's
present leaves at fork time, and a page that has not been faulted in yet is not a
leaf — so each side would demand-page its own private frame and the mapping would
silently behave like `MAP_PRIVATE` again. Sharing a page that does not exist yet
needs a backing object this target does not have. **Pinned divergence**, stated
at the branch in `sys_mmap`.

---

## Verification

### Boot self-tests

| rig | before | after |
|---|---|---|
| qemu/tcg `SMP=1` | 295 / 0 | **323 / 0** |
| qemu/tcg `SMP=4` | — | **332 / 0** |
| firecracker `SMP=4` (the box) | — | **322 / 0** |

The suite gained 28 checks, and one of them is load-bearing in a way the others
are not. `mm::smoke_test` used to check **only refusals**, on the reasoning that
the success path is exercised for real by every allocating program. Removing four
of those refusals would have left that reasoning holding up nothing — demand
paging could regress to never firing and every other check would stay green,
because an eagerly-populated mapping works too. So `mm::demand_paging_report`
runs *after* `busybox_test` / `execve_test` / `fork_test` / `redirect_test` and
asserts that at least one not-present fault was serviced out of a region:

```
mmap: user pages demand-paged from a region 8
mmap: the lazy path was actually taken   [OK]
```

`va_placement_check` replaces the removed `MAX_MAPPING` / `MAP_FIXED` arms with
six checks on the VA placer itself, including an **order-independence** case: the
region list is not kept sorted (`detach` pushes survivors onto the end), and a
gap scan that assumed sorted input would pass every other case and fail that one.

### The userspace probes

`userspace/forktest/c_stress/` — ~40 probes, each calibrated against real Linux.
They are architecture-neutral C, so the only aarch64-specific thing about them
was the compiler name; `scripts/mem_suite.py` takes `--arch x86_64` now and
writes to `c_stress/x86_64/`.

| probe | verdict | why |
|---|---|---|
| `mmap_stress` | **PASS** | |
| `madvshared` | **PASS** | |
| `shmanon` | **PASS** | was failing — see above |
| `cowstale` | **PASS** | |
| `mmapsum` | known | `pread64` (x86_64 17) not implemented here |
| `mmap_file` | known | file-backed `mmap` is `ENOSYS` by design |
| `mprotectlb` | known | needs a `SIGSEGV` handler; no signal delivery |
| `mremapmove` | known | `mremap` not implemented |
| `eager_mprotect_probe` | known | see below |
| `smapsdirty` | known | no `/proc/self/smaps`, no `MADV_FREE` |

**No failure is a memory-mapping defect.** The two that look like one are worth
recording, because both would be misread:

- `eager_mprotect_probe` forks a child, has it write to an `mprotect`ed page and
  checks `WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV`. On this target a
  killed process exits with **code** `128 + SIGSEGV` rather than reporting a
  *signalled* status, so that test can never be true. It prints only
  `RESULT: FAIL` because the child's own diagnostic is lost to `_exit`, which
  does not flush stdio.
- `mprotectlb` needs a `SIGSEGV` **handler** to survive its own probe. It dies
  with 139 instead — which is itself evidence the downgrade took effect.

`mprotect` was verified directly instead: `mmap` RW, touch, `mprotect(PROT_READ)`,
write ⇒ the process dies with 139. Before this work that write succeeded, because
`mprotect` was `return 0`.

Both of those are trunk **A2** (signals), not B.

---

## Harness work this forced, and three gaps it exposed

Running the suite on this target needed three things fixed in the harness, and
each of them is a real property of the guest rather than a harness bug:

1. **`2>&1` fails in the guest shell** — `/bin/sh: 1: Bad file descriptor`, exit
   1. `mem_suite.py` appended it to every probe command, so all ten failed before
   they started. The redirect was redundant (the runner merges both streams
   anyway) and is gone.
2. **busybox here has no `base64` applet**, and the failure is silent in the
   worst way: `base64 -d > /tmp/x` leaves a **zero-byte** file, because the shell
   creates the redirect target before discovering the command does not exist.
   `push` probes for the applet once and sends raw bytes when it is missing.
3. **There is no `/dev`**, so `stage()`'s `dd if=/dev/zero` produced nothing and
   both file probes failed with `open/fstat failed` — which reads like an mmap
   defect. The bytes are written over the ssh channel now.

For the Firecracker arm there is no ssh at all: `hpbox.firecracker` boots with
`"network-interfaces": []`. `scripts/utils/amd64_mem_trials.py` runs both
machines in parallel over the **console** instead, injecting the probes into the
disk image with `debugfs` and importing `mem_suite.verdict` rather than
re-deriving it.

That runner cannot use a shell script, and finding out why cost a cycle:

- **`sh <script>` cannot spawn anything here.** `sh -c <one command>` works
  because busybox ash execs it in place without forking; a *script file* fails at
  the first line that runs a program with `sh: <line>: Invalid argument`.
- **The init shell's own stdout goes nowhere.** `echo` from inside such a script
  produces no console output while its *errors* do — so the banners a harness
  splits on would be missing even if the probes ran.

`amd64/probes/run_all.c` routes around both with the three things that
demonstrably work: `fork`, `execve`, `wait4`. Its markers go out through
`write(2)`, not stdio — a marker still sitting in a buffer when a probe
deliberately SIGSEGVs is a marker the harness never sees.

A note on the reporter itself: its first version let `EXPECTED_FAIL` excuse a
probe that had **not run**, and printed six reassuring `known` lines for a boot
that executed nothing. `NOT REACHED` now beats the excuse. That is the
silent-pass trap wearing a different hat, and it got in anyway.

---

## What is still open here

- **File-backed `mmap`** — `ENOSYS`. Needs a page cache; the last B-trunk item
  and probably its own step.
- **`mremap`**, **`madvise(MADV_FREE)`**, **`pread64`** — not implemented. Not
  B1/B2, but each is one probe away from being needed.
- **VA reuse gives up a real property.** The bump allocator never handed out an
  address twice, so a use-after-`munmap` faulted rather than landing in a later
  mapping. First-fit only reuses a hole something was explicitly unmapped from,
  and the window is 112 TiB, so an address is recycled long after it went away —
  but it is recycled.
- **CoW is still `SMP=1`-only** (`docs/archive/AKUMA_AMD64_COW.md`). Lazy regions
  did not widen that: `fault_in` maps a fresh frame and demotes nothing, so it
  adds no cross-core stale-translation window.

## Background

- `proposals/NEXT_AGENT_AMD64_MMAP.md` — the brief, including the six traps this
  tree had already paid for.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the tree this is B1/B2 of.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — why `prot_recorded` exists.
- `docs/archive/AKUMA_AMD64_COW.md` — the pinned CoW-marker divergence.
- `docs/runbooks/amd64-bare-metal-loop.md` — how to run the probes on both rigs.
