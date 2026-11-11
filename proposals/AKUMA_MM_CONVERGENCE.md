# One memory-management layer for both kernels

*2026-11-11, branch `akuma-multiarch-attempt-0`. Slice 5 of the convergence in
[`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`](../docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md),
deferred there because it cannot be done one call at a time.*

## Status

| step | state |
|---|---|
| 1. `LazyRegion` stores `Prot` | **done** — `Option<Prot>`, the `u64` only at `LazyRegionMap`'s edges (exact, host-tested); boot suites, `mem_suite` 11/11 |
| 2–6 | open |

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
has an empty dependency table on purpose. The pin set amd64 already keeps in its `UserAddressSpace`
(one pin per distinct inode, released when the address space drops) moves into the shared
`UserAddressSpace`, so AArch64 gets the same mechanism.

This changes AArch64 behaviour: today a pin goes away when its region is unmapped; after this, an
unlinked file that was mapped keeps its blocks until the process exits or execs. That is bounded by
the number of distinct files a process maps, and it is already amd64's behaviour. Releasing a pin when
the last region naming its inode goes is possible, since `munmap` returns the detached regions. It is
a follow-up if a long-lived process is ever found to hold unlinked files.

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
