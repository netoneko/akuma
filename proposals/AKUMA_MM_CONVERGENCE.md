# One memory-management layer for both kernels

*2026-11-11, branch `akuma-multiarch-attempt-0`. Slice 5 of the convergence in
[`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`](../docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md),
deferred there because it cannot be done one call at a time.*

## Status

| step | state |
|---|---|
| 1. `LazyRegion` stores `Prot` | **done** — `Option<Prot>`, the `u64` only at `LazyRegionMap`'s edges (exact, host-tested); boot suites, `mem_suite` 11/11 |
| — | **`MAP_FIXED` leak fixed first** (2026-11-12): glue's `MAP_FIXED` kept every replaced eager frame until exit and left an overlapping record in the list. It now goes through `unmap_range`, `munmap`'s body. A prerequisite for step 5, since this `mem.rs` becomes amd64's mm too ([`AKUMA_MAP_FIXED_EAGER_LEAK.md`](../docs/archive/AKUMA_MAP_FIXED_EAGER_LEAK.md)) |
| 2 | folded into step 4 (below) |
| 3. pin set in the shared `UserAddressSpace` | **done** (2026-11-12): `akuma_user_space::MappingPins` held by both structs, methods shared. AArch64 pins on `push_lazy_region_with_source`, fork's `propagate_lazy_regions_to_child`, exec and spawn (deferred segments), and releases when `munmap` removes the last region naming the inode. Thread views (`clone_lazy_regions`) pin nothing. Boot test `test_lazy_file_regions_pin_in_address_space`; `ext2probe` `SPACE OK`; `mem_suite` 12/12 |
| 4a. `MmapRegion` can describe a demand-paged ELF segment | **done** (2026-11-12): `lazy` marker, `FileBacking::lead`, `page_span` |
| 4b-i. a lazy file region is named by `(mount, inode)` | **done** (2026-11-12): no path in `LazySource`; inode-less files map eagerly |
| 4b-ii. lazy regions join `mmap_regions` | **done** (2026-11-12): `LazyRegionMap`, `Process::lazy_regions`, `clone_lazy_regions`, `update_lazy_region_flags` and `propagate_lazy_regions_to_child` deleted; the `*lazy_region*` names are views over the one list. fork reads the leader's list (a worker's fork used to read its own, empty one); a thread's faults resolve through the address-space owner. A replacing push now releases the old file's pin. Gates: AArch64 HVF boot suite 318/326/324 (SMP 1/2/4, unchanged), `mem_suite` 12/12, `dynspawn` 0 divergences, `ext2probe` SPACE OK (100 % returned, pinned and unpinned), forktest stress, `futextest`/`thrchurn`/`sigprobe`, clean `-j4` self-host kernel build in the devbox VM (EXIT=0, 1m12s, tripwires 0) |
| 5a. `mprotect` is glue's on both kernels | **done** (2026-11-12): amd64's copy deleted; `UserAddressSpace::reprotect_present` on both walkers; amd64's CoW rule adopted, which fixed AArch64 granting a write to a forked private page (`cowprotect` probe, [`AKUMA_MPROTECT_CONVERGENCE.md`](../docs/archive/AKUMA_MPROTECT_CONVERGENCE.md)). Gates: `mem_suite` 13/13 on both, amd64 TCG suite SMP 1/2/4, AArch64 devbox self-host `-j4` EXIT=0 |
| 5b. one VA placer | **done** (2026-11-12): amd64's cursor + region-gap first fit moved to `akuma_mmap::place`; AArch64 uses it through `ProcessMemory::place`, and the free list (`free_regions`, `alloc_mmap`, `free_mmap`, `vm_free_mmap`, the "recycle only if it had frames" rule) is deleted. Placement and a `PROT_NONE` reservation share one region-lock hold ([`AKUMA_ONE_VA_PLACER.md`](../docs/archive/AKUMA_ONE_VA_PLACER.md)). Gates: both boot suites SMP 1/2/4, `mem_suite` 13/13 on both, devbox self-host `-j4` EXIT=0, `thrchurn`/`spawnalias`/`clonearg` |
| 5c. `munmap` is glue's on both kernels | **done** (2026-11-12): amd64's `sys_munmap` and `unmap_range` body deleted; glue's `unmap_range` is one detach of every region (eager and lazy, file pins released) and one live-PTE sweep (`UserAddressSpace::unmap_present` on both walkers). amd64's shared writable table stays its own behind the `release_shared_write` hook until step 6 ([`AKUMA_ONE_MUNMAP.md`](../docs/archive/AKUMA_ONE_MUNMAP.md)). Gates: both boot suites SMP 1/2/4, `mem_suite` 13/13 on both, devbox self-host `-j4` EXIT=0 |
| 5d. `mmap` is glue's on both kernels | **done** (2026-11-12): amd64's `sys_mmap`, its file-descriptor probes and the `mm-forensics` feature deleted; amd64 keeps `/dev/fb0` and the `/dev/kvm` census in its dispatcher and its shared writable table behind the `shared_write_backing` hook. amd64's rules adopted by both: the descriptor refusals, `MAP_FIXED_NOREPLACE`'s `EEXIST` (fixed on AArch64: `mapnoreplace`), shared anonymous never lazy; the kernel hole is per kernel (`ProcessMemory::kernel_hole`) ([`AKUMA_ONE_MMAP.md`](../docs/archive/AKUMA_ONE_MMAP.md)). Gates: both boot suites SMP 1/2/4, `mem_suite` 13/13 on both, `shmwrite`/`shmcoh`/`shmshare`/`shmvar`/`shmregionprobe`/`mapnoreplace` on amd64 (all pass) and AArch64 (unchanged from 5c except `mapnoreplace`, now passing), devbox self-host `-j4` EXIT=0; amd64 self-host on the ryzen (Firecracker guest over p3, `kbuild -c -j 4`) EXIT=0, 127 crates, 1m05s |
| 5e. `mremap` and `madvise` are glue's on both kernels | **done** (2026-11-12): amd64's `sys_mremap`, `sys_madvise` and `dontneed_range` deleted. Glue's `mremap` takes amd64's design — re-point present pages (`UserAddressSpace::move_present` on both walkers), carry the region's records, refuse `MREMAP_FIXED` — where AArch64 allocated and copied. Glue's `MADV_DONTNEED` takes amd64's rules: only region and `brk` pages, never a device page, replacement frames mapped with the region's protection ([`AKUMA_ONE_MREMAP_MADVISE.md`](../docs/archive/AKUMA_ONE_MREMAP_MADVISE.md)). Gates: both boot suites SMP 1/2/4, `mem_suite` 13/13 on both, the `shm*`/`mapnoreplace`/`altstackexec` probes on amd64 (all pass) and AArch64 (unchanged from 5d), devbox self-host `-j4` EXIT=0 (1m05s), ryzen amd64 self-host EXIT=0 (127 crates). `msync` and `brk` stay per kernel until step 6 |
| 6. Writable shared file pages are one table on both kernels | **done** (2026-11-12): amd64's `shmpages.rs` moved to `akuma_exec::shm` (both fault paths call it), glue owns write-back, release and `msync`, and glue's `SHARED_FILE_MAPPINGS` and the two backing hooks are deleted. AArch64 gains cross-process coherence: every `shm*` probe passes there now, where all failed before ([`AKUMA_ONE_SHARED_WRITABLE_TABLE.md`](../docs/archive/AKUMA_ONE_SHARED_WRITABLE_TABLE.md)). Gates: both boot suites SMP 1/2/4 (AArch64 −1 test, the deleted writeback test), `mem_suite` 13/13 and the `shm*` probes on both, devbox self-host `-j4` EXIT=0 (1m09s), ryzen amd64 self-host EXIT=0 (127 crates). A write-back race found the same day is fixed in the archive doc's correction |
| 7. `brk` is one implementation | **done** (2026-11-12): `Process::set_brk` for both kernels, refusing growth over any mapping, past free memory or on OOM (undone), and unmapping on shrink. amd64 grows the break for the first time; AArch64 lost its lazy heap region and the loader's 16 pre-mapped heap pages ([`AKUMA_ONE_BRK.md`](../docs/archive/AKUMA_ONE_BRK.md)). Gates: both boot suites, `mem_suite` 14/14 with the new `brkgrow` on both, devbox self-host EXIT=0, ryzen amd64 self-host EXIT=0 (127 crates). Every call in the goal now has one implementation |

## Goal

`mmap`, `munmap`, `mprotect`, `mremap`, `madvise`, `msync` and `brk` are served by **one**
implementation, `akuma-syscalls-glue`'s `mem.rs`, on both kernels, over **one** region model. What
stays per architecture is the machine half:

- the page-fault fill: allocate, read, install;
- I-cache maintenance;
- the PTE encoding of a `Prot`;
- device mappings (`/dev/fb0`, `/dev/kvm`).

Out of scope: merging the two fault paths into one. They read the same regions after this work; the
fill algorithms (AArch64's three-pass fill with the BKL dropped, amd64's `fault_in`) can converge
later, with their own measurements.

## Where the two kernels stand

Both kernels already share `akuma_exec::process::Process`, `akuma_mmu::UserAddressSpace` (AArch64
walker and x86 walker behind one name), `akuma-mmap`'s region algebra, `akuma_mmap::Prot` and
`akuma_syscalls_mem::mmap::plan` (which kind of mapping a request asks for). The split is in what
they write into `Process`:

| | AArch64 (glue `mem.rs`, `akuma-exceptions`) | amd64 (`amd64/src/mm.rs`, 3.5 kL) |
|---|---|---|
| eager mappings | `Process::mmap_regions` (`MmapRegion`, `prot: Prot`) | `Process::mmap_regions` |
| demand-paged mappings | `Process::lazy_regions`, a `BTreeMap` of `LazyRegion`, keyed by tgid through `push_lazy_region(pid, ..)` | the same `mmap_regions` list: a region is lazy simply by having pages not yet present |
| protection of a lazy mapping | `LazyRegion::flags: u64`, **AArch64 PTE bits** | `MmapRegion::prot: Prot` |
| file identity | `LazySource::File { path, mount_id, inode, file_offset, filesz, segment_va, pin }` | `MmapRegion::file: Option<FileBacking>` (mount, inode, offset of region start, bytes of file from region start) |
| inode lifetime | an `InodePin` inside each `LazySource`, released when the region drops | `UserAddressSpace::pin_mapped_inode`, one pin per inode, held until the address space drops |
| writable `MAP_SHARED` file | eager, private frames, glue's `SharedFileMapping` table, whole-region write-back | lazy, **one frame per page across mappers** (`akuma-fpcache-rw`, `shmpages`), `MmapRegion::shared_write` |
| fault reader | `akuma-exceptions` via `lazy_region_lookup_for_page_fault` (15 call sites) | `mm::fault_in` via `region_index_containing` |
| file readahead | 256 pages, shared read-only pages from `akuma-fpcache` | 16 pages, also `akuma-fpcache` |
| `brk` | glue `sys_brk` grows the heap | returns the current break, never grows (musl falls back to `mmap`) |
| `msync` | glue | `ENOSYS` row in the dispatcher; `mm::sys_msync` exists but is not routed |

So routing amd64's `mmap` to glue today would write `LazyRegion`s that amd64's `fault_in` never
reads, and AArch64 bits amd64's walker cannot decode. The region model has to become one before any
arm moves.

## Target model

**One list.** Every mapping, eager or lazy, is an `MmapRegion` in `Process::mmap_regions`, sorted,
with:

- `prot: Prot` as its only permission record;
- `file: Option<FileBacking>` for a demand-paged file mapping;
- `shared_write` for a writable shared file mapping.

`Process::lazy_regions`, `LazyRegion`, `LazySource` and `LazyRegionMap` are deleted. Splits, clips
and fork inheritance are `akuma-mmap`'s existing operations, which already carry `file` through every
reshape (`FileBacking::advance`, host-tested).

**The fault path asks one question.** `akuma_exec::process::fault_region(va) -> Option<FaultRegion>`
answers it, with `{ start, pages, prot, file, shared_write }` copied out under the list's lock. Both
kernels' fault paths consume that and nothing else; AArch64 encodes `prot` with
`user_flags::to_pte`, amd64 with `PteProt::from_region`.

**Inode lifetime moves to the address space.** `MmapRegion` cannot hold an `InodePin`; `akuma-mmap`
has an empty dependency table on purpose. The pin set amd64 keeps in its `UserAddressSpace` (one pin
per distinct inode, released when the last region naming it goes) moves into the shared
`UserAddressSpace`, so AArch64 gets the same mechanism and the same lifetime it has today. (An earlier
draft of this section said amd64 held pins until the address space dropped; amd64 already released
them through `retain_mapped_inode_pins`.)

**File reads by path survive only where they must.** `LazySource` keeps a path so a fill can
`read_at(path, ..)` when the inode is 0. `FileBacking` has no path. Every mapping glue creates has an
inode (`resolve_file_extent`). The ELF loader's segments need checking; a segment with no inode
stays eager rather than adding a path to `FileBacking`.

**Writable `MAP_SHARED` file mappings converge on amd64's model**, one frame per page across mappers,
because it is the stronger one: SQLite's WAL index needs it, and AArch64's per-process eager copies
are the shape that corrupted goose's database on amd64. This is the last step, after both kernels
route through glue, and until then it is a glue hook with two implementations.

## Steps

Each step is one commit, keeps both kernels building and passing, and changes one thing.

1. **`LazyRegion::flags` becomes `prot: Prot`.** AArch64 only, behaviour-identical: the fault path
   encodes with `to_pte` where it read the word, and `push_lazy_region` keeps its `u64` signature as a
   `from_pte` wrapper so the 33 boot-test callers do not change. `from_pte`/`to_pte` round-trip
   exactly on all six encodings this tree produces (`from_pte_inverts_to_pte`). This finishes
   `REDUCING_PLATFORM_DEPENDENCY.md` §1.
2. **One fault question.** `fault_region(va)` replaces `lazy_region_lookup_for_page_fault` in
   `akuma-exceptions`, answering from `lazy_regions` for now, with `LazySource` translated to a
   `FileBacking` plus the path. Pure plumbing; the 15 fault-path call sites stop naming
   `LazySource`.
3. **The pin set moves into the shared `UserAddressSpace`.** AArch64 regions start pinning through
   it alongside their `InodePin`, so nothing is lost when the `InodePin` goes in step 4.
4. **Lazy regions join the one list.** `push_lazy_region*` insert `MmapRegion`s; munmap, mprotect,
   mremap, fork and exec go through `akuma-mmap`'s operations on `mmap_regions`; `fault_region`
   answers from the list; `Process::lazy_regions` is deleted. The `*lazy_region*` functions stay as
   names over the list, so the boot tests prove the behaviour held. **This is the step that can
   break self-host**, and its gate is an AArch64 self-host kernel build.
5. **glue `mem.rs` is the one implementation.** amd64 routes `mmap`, `munmap`, `mprotect`,
   `mremap`, `madvise`, `msync` and `brk` to glue. amd64 keeps:
   - its device mappings, through a chardev hook (`CharDevOps` gains `mmap`);
   - its shared-writable pages, through a `shared_write` hook;
   - its eager fill and `prefault_user_range`, if glue's `UserAddressSpace` calls cannot express
     them yet.

   Every divergence found by diffing the two copies is pinned in the commit, not silently resolved.
   That includes:
   - readahead (16 vs 256);
   - `brk` growing on amd64, which it never has;
   - `MAP_SHARED|MAP_ANONYMOUS`;
   - the `MADV_DONTNEED` rules;
   - `mremap`'s move rules.
6. **Writable shared file pages converge** on `akuma-fpcache-rw` for both kernels, and glue's
   `SharedFileMapping` table is deleted.

## Decisions after step 1

Reading the code for step 2 changed four parts of the plan above.

- **Step 2 folds into step 4.** `LazySource` is what the AArch64 fault path already consumes, in
  three passes with the BKL dropped. Translating it to a `FileBacking` and back in step 2, only
  to delete the source in step 4, is two rewrites of the 15 call sites. Instead, step 4 keeps
  `LazySource` as the fault path's *view*, built from the one list under its lock, so the fault
  code does not change in the step that moves the storage.
- **Pins are released with the last region that names the inode.** Both kernels already had that
  lifetime: AArch64 through each region's `InodePin`, amd64 through `retain_mapped_inode_pins`. The
  set lives in the shared `UserAddressSpace` (step 3, done). `munmap`, `MAP_FIXED` and `mremap`
  release a pin through the regions they detach, and since step 4b-ii so does a push that
  replaces part of a lazy region.
- **`MmapRegion` needs a `lazy` marker.** Three AArch64 readers of the eager list assume
  every record in it is backed by present pages:
  - `eager_region_flags_for_page_fault`;
  - `recorded_protection`;
  - `munmap`'s frame free.

  Once lazy regions join that list, each reader must skip lazy records or treat them
  differently, and the marker is what lets them tell.
- **ELF segments start their file data above the region start** (`segment_va >
  start_va`: the first page holds a partial segment). `FileBacking`'s "offset of the region
  start" cannot express that, so step 4 extends `FileBacking` rather than dropping the field.
  Lazy file regions with inode 0 (the `read_at(path, ..)` fallback) must be audited in the same
  step: list every producer and confirm it always has an inode, or keep it eager.

## Gates

| step | gate |
|---|---|
| every step | clippy `-D warnings` on AArch64, amd64 (default, `headless`, `no-tests`) and Taoxichuan; host tests (`akuma-mmap`, `akuma-mmu`, `akuma-syscalls-mem`, `akuma-exec`); AArch64 HVF suite SMP 1/2/4; amd64 TCG suite SMP 1/2/4 |
| 1–4 (AArch64 fault path) | `scripts/mem_suite.py` (the ten `c_stress` memory probes), `fdrefuse`/`futextest`/`thrchurn`, and for step 4 an in-VM self-host kernel build (`scripts/run_selfhost_kernelbuild.py`) |
| 5–6 (amd64 mm) | the same probes on amd64, the amd64 boot suite's `mm` checks (`smoke_test`, `mremap_check`, `madvise_check`, `va_placement_check`), and the Firecracker self-host lane before the slice counts as done |

## Risks

- **Step 4 is the change.** `lazy_regions` is read under its own spinlock from the fault path, and
  `mmap_regions` is an `AssistedSpinlock` with its own lock order (regions, then address space, then
  PMM on amd64). The AArch64 fault path drops the BKL during its fill. Before step 4 lands, write
  down which locks it holds when it looks a region up and confirm the list lock fits the documented
  order (`docs/reference/subsystems/locking.md`). Do not discover it at runtime.
- **Lookup cost.** amd64 measured ~4 400 regions in a `rustc` and uses a binary search for that
  reason. Lazy regions on AArch64 were a `BTreeMap` keyed by start; the sorted list with
  `region_index_containing` costs the same.
- **The archive is full of lazy-region bugs** (`FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV`, the inode
  lifecycle ICE, `LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG`). Each established an invariant that step 4
  must keep. Read them before step 4, and turn each into a check in the step's commit.

Background: [`REDUCING_PLATFORM_DEPENDENCY.md`](../docs/archive/REDUCING_PLATFORM_DEPENDENCY.md) §1;
[`AKUMA_EXTRACT_MMAP.md`](../docs/archive/AKUMA_EXTRACT_MMAP.md);
[`AKUMA_MULTIARCH_ATTEMPT_0.md`](../docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md) "Slice 5: mm".
