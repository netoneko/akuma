# mem syscalls

mmap / munmap / brk / mremap / mprotect / madvise / msync / membarrier. Source:
`src/syscall/mem.rs`. For PMM, the kernel heap allocator, CoW fork, lazy
regions, the mmap bump allocator, MAP_FIXED, lazy anonymous mmap, and page
eviction, see [`../memory.md`](../memory.md) — this doc covers only the
syscall entry-point layer: argument validation, error codes, and quirks
visible at the syscall boundary.

> **Stability: B (verify behaviour).** Upgraded from C on 2026-08-29: the
> family's decisions and its region algebra now have 62 host tests behind them
> (`akuma-syscalls-mem` 41, `akuma-mmap` 21) and a live gate that runs all ten
> probes (`scripts/mem_suite.py`), where before there was neither. Verified the
> same day on **both** platforms — QEMU (3 boots, 0 FAILED) and Firecracker under
> Lima (307 PASSED, 0 FAILED, 0 POISON) — with `mem_suite` 10/10, and benchmarked
> against real Linux (see below).
>
> Still not A, and the reasons are specific: region-boundary bugs surface here
> first, and the fault
> path — where the measured gap to Linux actually is — is untouched by any of the
> above. ~~`cowstale` is a known open flake that also fails on `main`~~ —
> **corrected 2026-08-30**: the stale-write-fault residual got its second fix
> stage (absorb re-checks after the fault-slot wait and at SIGSEGV delivery,
> `archive/COWSTALE_FORK_THREAD_SEGV.md` header); at SMP=4 in-boot it went
> hammer 1/15, classic 0/8, but the hammer survivor keeps the class off grade-A
> until explained.
>
> The recurring lesson: **validate arguments before calling `lookup_process`** —
> an `EINVAL`/`EFAULT` check done first keeps a kernel-test caller (no current
> process) from getting `ESRCH` instead of the argument error Linux expects.
> Two 2026-08-29 defects were the same lesson at a different scale: `len` was
> not validated at all, so `mmap(len=-1)` and `madvise(len=-1)` became unbounded
> kernel loops reachable from unprivileged userspace.

## Where the logic lives (2026-08-29)

The family's pure decisions and its region algebra were extracted so they could be
host-tested instead of boot-tested. Three homes, and the split is the point:

| what | where | why there |
|---|---|---|
| mapping-kind plan (lazy/eager, file-backed, shared-writable, `shared_anon`), `MAP_FIXED` validation, `munmap` sizing, mremap's move-vs-expand, madvise's advice decode, `MADV_DONTNEED`'s range + per-page rule, membarrier decode | [`crates/akuma-syscalls-mem`](../../../../crates/akuma-syscalls-mem) | pure over the argument bits; **never sees a region**, which is what keeps it a leaf beside `-sync` and `-poll` |
| `MmapRegion`, `PhysFrame`, CoW-fork inheritance, `munmap`'s clip-and-split, the PTE permission vocabulary (`user_flags`, `is_write`) | [`crates/akuma-mmap`](../../../../crates/akuma-mmap) | region *algebra*; zero dependencies, so it cannot lock, allocate or name a `Process` |
| every probe, lock, frame, page-table edit, TLB flush, user-memory access; `Process::vm_lock` / `vm_with_regions`; `dontneed_count_shared` / `dontneed_apply`; the fault path | `src/syscall/mem.rs`, `src/exceptions.rs` | effects, and arguments about locking and hardware |

If a change to `akuma-syscalls-mem` finds itself wanting `MmapRegion`, the seam is
drawn in the wrong place — move the seam, do not add the dependency.

## Known divergences from Linux

Preserved on purpose and pinned by a test named to say what it is, so an
extraction stays A/B-able against what it replaced. Numbering matches
`akuma_syscalls_mem`'s crate docs.

| # | divergence | pinned by |
|---|---|---|
| 1 | `munmap(addr, 0)` unmaps **one page**; Linux returns `EINVAL` | `diverge_munmap_zero_length_unmaps_one_page` |
| 2 | `MADV_DONTNEED` with an unaligned start rounds **down**, clearing a strict superset of Linux's range including the caller's partial head page; Linux returns `EINVAL` | `diverge_unaligned_start_rounds_down_and_covers_the_head_page`, counted by `DONTNEED_UNALIGNED` |
| 3 | `MADV_FREE` returns `EINVAL` — deliberate: Redis reads it as "older kernel" and starts, where a fabricated 0 sends it into a self-check it cannot pass without `/proc/<pid>/smaps` | `diverge_madv_free_is_einval_not_success` |
| 4 | every other advice returns success without acting, including ones Linux implements | `diverge_unknown_advice_reports_success` |
| 5 | a shrinking `mremap` returns the old address and leaves the tail mapped; Linux unmaps it | `diverge_shrink_is_in_place_and_leaves_the_tail_mapped` |
| 6 | `MAP_FIXED_NOREPLACE` is alignment-validated then treated as `MAP_FIXED`, without the "fail if occupied" check | — |
| 7 | `membarrier` recognises only `QUERY`/`PRIVATE_EXPEDITED`/`REGISTER_PRIVATE_EXPEDITED`; `GLOBAL` (1) and the `SYNC_CORE` family are `EINVAL` | `diverge_global_and_unknown_commands_are_einval` |

`/proc/<pid>/` is not populated (no `smaps`, `maps`, `statm`), which `smapsdirty`
reports as DIVERGE rather than FAIL.

## Testing

```bash
# Host, milliseconds — the decisions and the region algebra
cargo test -p akuma-syscalls-mem -p akuma-mmap --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)

# Live guest — all ten c_stress memory probes
scripts/mem_suite.py --port 2322            # add --only <a,b> for a subset

# Cost, A/B/A, ratio-to-getpid not ns
scripts/benchmarks/mem_ab_run.sh <label> <outdir> 4
```

`mem_suite.py` refuses to score a **silent** probe as a pass — output must exist,
exit must be 0, no `FAIL` word — and treats `DIVERGE` as green. `mem_op_cost`
takes a third argument `hostile` (default 1); `hostile=0` skips the two arms a
pre-2026-08-29 kernel cannot survive, so a baseline A/B arm can still run.

### Where this family stands, measured (2026-08-29)

`mem_op_cost` is built as one static musl binary and run on **both** kernels, so
the code is identical and only the kernel differs. Ratios are to each kernel's own
`getpid`, because Akuma runs under QEMU and the Linux baseline under Apple `vz` —
absolute nanoseconds across the two are not comparable.

| arm | Akuma | Linux | Akuma / Linux |
|---|---:|---:|---:|
| `mremap_inplace` | 0.97x | 1.30x | **0.75x** |
| `mmap_einval` | 1.00x | 1.18x | 0.85x |
| `mremap_efault` | 1.00x | 1.18x | 0.85x |
| `madv_willneed` | 1.24x | 1.46x | 0.85x |
| `membarrier` | 0.94x | 0.97x | 0.97x |
| `brk_query` | 1.22x | 1.02x | 1.20x |
| `mprotect_noop` | 2.76x | 1.34x | **2.06x** |
| `munmap_noent` | 2.99x | 1.12x | **2.66x** |
| `madv_unmapped` | 4.38x | 1.61x | **2.72x** |

**The split is clean and it says where to work.** Every *decode* path — the part
`akuma-syscalls-mem` owns — is at or better than Linux. The whole remaining gap is
the three arms that do real page work: TLB maintenance, region bookkeeping and the
per-page walk. Nothing in the extracted crates is on the critical path for those.

The most recent win there shows the shape of what is available: `madv_unmapped`
was **13.94x** before `madvise_dontneed_range` stopped taking a lock per page
(`docs/archive/CONSOLE_LOG_COST.md` §9), and is 4.38x now — still 2.72x Linux.

Syscall floor for scale: Akuma 152 ns, Linux 105 ns (1.45x, different
hypervisors). `getuid` is `FastPath::Leaf` and costs **0.72x** Akuma's own
`getpid`; the same call on Linux is 1.00x of its own — that tier has no Linux
counterpart.

Reproduce the Linux side with `userspace/memprobe/c/build.sh --push-lima fc`.

**A `mprotect` bug this gate found — fixed 2026-08-29, and the fix is four
changes because the bug was four writers.** A child writing to a page its parent
had `mprotect(PROT_READ)`-ed did not `SIGSEGV`: the EL0 write-fault handler's
CoW-break arm fires on `cow_ref > 0` alone and hands the writer a private
*writable* copy. A CoW-demoted page and an `mprotect`-demoted page are both
read-only in the PTE; the region's recorded protection is what distinguishes them,
and the CoW arm never consulted it.

Gating the CoW break on that record is correct in principle and broke `rustc`
three times in a row in practice, because **`MmapRegion::flags` had only ever been
read to GRANT a write, and every writer was sloppy in a way that is invisible to a
granting reader and fatal to a denying one**:

| writer | wrote | denied |
|---|---|---|
| `MmapRegion::owned()` | `NONE` meaning "unrecorded" | every region built without explicit flags |
| `update_eager_region_flags` | a sub-range `mprotect` against the **whole** region | a guard page's neighbours |
| `sys_mremap` | `old_flags.unwrap_or(NONE)` | every `mremap` of a lazy or sub-range source — i.e. every `realloc` |
| `fork`'s region copy | the value without "was it recorded" | a child of an unrecorded parent |

All four are fixed: `MmapRegion::prot_recorded` / `recorded_prot()` separate a
statement from a default; `mprotect` now **splits** the region
(`akuma_mmap::mprotect_eager_regions_in_range`) so a piece's flags describe that
piece alone; `mremap` produces `MmapRegion::owned()` for an unknown source; and
`fork` carries `prot_recorded`. Removing any one brings the crash back.

The bug class, the three failed fixes, and the two structural tells that a record
is grant-only are written up in
[`archive/GRANT_RECORDS_VS_DENY_RECORDS.md`](../../../archive/GRANT_RECORDS_VS_DENY_RECORDS.md).
Read it before adding a reader to any permission record.

Verified: `eager_mprotect_probe` both phases, and an in-VM `cargo build --release`
of Akuma in devbox-smoltcp — the reproducer that killed `thiserror-impl` and
`zerocopy-derive` on every run — now finishes in 1m 39s with **0 SIGSEGV and 0
`[MPROTECT-DENY]`**.

Probe: `userspace/forktest/c_stress/eager_mprotect_probe`.

## Syscall table

All always-on (no `sc-*` gate; see [`../syscalls.md`](../syscalls.md) "The
`src/syscall/` split").

| Syscall | nr | Entry point |
|---|---|---|
| `brk` | 214 | `sys_brk` |
| `munmap` | 215 | `sys_munmap` |
| `mremap` | 216 | `sys_mremap` |
| `mmap` | 222 | `sys_mmap` |
| `mprotect` | 226 | `sys_mprotect` |
| `msync` | 227 | `sys_msync` |
| `madvise` | 233 | `sys_madvise` |
| `membarrier` | 283 | `membarrier_cmd` |

## Argument validation & error codes

**`mmap`** (`sys_mmap`, `mem.rs:189`):
- `len == 0` → `EINVAL`.
- `MAP_FIXED`/`MAP_FIXED_NOREPLACE` with `addr != 0` and `addr & 0xFFF != 0` →
  `EINVAL` (`mmap_fixed_addr_unaligned_einval`). This check runs **before**
  `lookup_process` specifically so a kernel-test caller with no current
  process still gets `EINVAL`, not `ESRCH` — a crash regression test
  (`crash14: addr = 0xffffffffffffffea`) pins this ordering.
- No current/owner process → `ESRCH`.
- `MAP_FIXED`/`MAP_FIXED_NOREPLACE` overlapping the kernel identity-map range
  (`mmap_fixed_overlaps_kernel_va`, scales with `kernel_va_end()`) → `EINVAL`.
  Guards against Go's runtime committing heap arenas with `MAP_FIXED` onto the
  kernel's RAM identity map (silent corruption otherwise).
- Bump allocator (`proc.memory.alloc_mmap`) exhausted for a non-fixed request
  → `ENOMEM`.
- Eager frame batch alloc fails, and reclaim-and-retry also fails: a writable
  `MAP_SHARED` file-backed mapping → `ENOMEM` (must stay eager to track pages
  for writeback — no safe lazy fallback); everything else silently degrades
  to a lazy region instead of failing (see `../memory.md` "Userspace memory
  model" — lazy anonymous mmap).

**`mremap`** (`sys_mremap`, `mem.rs:400`):
- `new_size == 0` → `EINVAL`; `old_addr` misaligned → `EINVAL`.
- `old_addr >= user_va_limit()` → `EFAULT`.
- Shrink (`new_pages <= old_pages`) → returns `old_addr` unchanged, a no-op
  (matches Linux; no copy performed).
- `!MREMAP_MAYMOVE` (grow in place only): `ENOMEM` if `old_addr` **is**
  mapped (can't satisfy an in-place grow) vs. `EFAULT` if it **isn't** mapped
  at all. This `ENOMEM`-vs-`EFAULT` split is deliberate and Linux-shaped —
  see Background: JSC's GC used to get `ENOMEM` for every unmapped probe
  address and couldn't distinguish "not my mapping" from "can't grow here".
- `MREMAP_MAYMOVE`, new region alloc fails → `ENOMEM`.

**`brk`** (`sys_brk`): no error path modeled. `new_brk == 0` is a size query
(returns the current brk). If the owner process can't be looked up, returns
`0` (not an errno) — brk's ABI has no failure return distinct from "brk
didn't move", so there's nothing to signal.

**`mprotect`** (`sys_mprotect`, `mem.rs:602`):
- `len == 0` → `0` (no-op success, matches Linux).
- `addr` misaligned → `EINVAL`.
- Owner process lookup fails → **`EINVAL`**, not `ESRCH`. This is
  inconsistent with `mmap`/`munmap`'s `ESRCH` on the same failure — a
  syscall-boundary quirk worth knowing if a newly-ported binary's mprotect
  path behaves differently than its mmap path under the same condition.
- Unmapped pages in the requested range are silently skipped (not an error);
  only mapped pages get `update_page_flags`.

**`munmap`** (`sys_munmap`, `mem.rs:654`): owner lookup fails → `ESRCH`.
`addr`/`len` are page-rounded; unmapping an address with nothing mapped there
is **not** an error (matches Linux — silently succeeds).

**`madvise`** (`sys_madvise`, `mem.rs:1001`): `MADV_FREE` returns `EINVAL`
(deliberately — see below); everything else returns `0` unconditionally, and OOM
during `MADV_WILLNEED` pre-faulting is silently swallowed.

`MADV_FREE`'s `EINVAL` is not a gap, it is the correct answer and load-bearing:
Redis probes `MADV_FREE`, reads `EINVAL` as "older kernel, presumably
unaffected", and starts — where a fabricated `0` sent it into a THP-corruption
self-check it cannot pass without `/proc/<pid>/smaps`
(`../../../archive/LONG_ROAD_TO_REDIS.md` §5). The **consequence** is that
`MADV_FREE`-probing allocators (jemalloc, mimalloc) fall back to
`MADV_DONTNEED`, so all of that traffic lands on the arm below.

### `MADV_DONTNEED` — breaks sharing, does not drop the mapping

`madvise_dontneed_range` (`mem.rs:938`). Per page, the rule is
`dontneed_page_action(mapped, cow_ref)`:

| `cow_ref` | What it means | Action |
|---|---|---|
| 0 | never shared | zero the frame in place, no allocation |
| 1 | the peer already went away (exited, or broke CoW itself) | zero in place |
| **≥ 2** | **another address space maps this frame** | **break the share** |

`cow_ref` counts **address spaces**, and the first share inserts 2
(`akuma_pmm::cow_ref_inc`), so 2 is the smallest value meaning "someone else can
see this frame". Breaking the share = `unmap_and_free_page(va)` — the usual
`released_last_va` gate, so `pmm::free_page` routes through `cow_ref_dec` and
declines to free the frame while the peer holds it — then a freshly zeroed
private frame mapped at the same VA, `RW_NO_EXEC`.

**Until 2026-08-14 every page in that bottom row was `memset` in place**, which
after a `fork` wrote through the frame the peer was still reading: the peer's
page went to zeroes, 0 of 4096 bytes surviving. That is the mechanism behind the
cargo null-`Rc` crash — cargo forks per rustc invocation, so its heap is exactly
that shape. (The corruption is proven; that the crash *took* this route is a
strong inference, and `dontneed_shared_frame` during a build is what settles it.)
See
[`../../../archive/MADV_DONTNEED_SHARED_FRAME.md`](../../../archive/MADV_DONTNEED_SHARED_FRAME.md).

**Why not Linux's drop-the-mapping.** Linux unmaps and lets the next touch
refault. That does not work here: **eager `mmap`s register no lazy region**
(≤ `config::MMAP_EAGER_MAX_PAGES` = 16 pages), so `ensure_user_page_mapped` would
have nothing to demand-page from and the next touch would be SIGSEGV rather than
a zero page. A private zero frame gives the caller the identical observable
result — mapped, readable, all zeroes — uniformly across eager and lazy mappings.

**Two passes, and why.** Frames must be allocated **outside** `as_lock` (the
PMM's reclaim path re-enters it and the `Spinlock` is not reentrant), but the
state that says how many are needed is in the page tables. So
`dontneed_count_shared` counts under the hold, the allocation happens between the
holds, and `dontneed_apply` re-reads every page and acts on what it finds *then*.
A peer that broke CoW inside that window is simply seen as unshared; a page that
became shared inside it (a concurrent `fork`) finds no spare frame and is
**skipped**, never wiped. The same skip absorbs a PMM that cannot serve the
batch — the advice is advisory, and failing to zero the caller's own page beats
zeroing someone else's.

**Counters** (`dontneed_audit_line`, in the PSTATS `[MADV]` line):

| Counter | Reads |
|---|---|
| `dontneed_shared_frame` | pages whose share was broken — before the fix, each one was a corruption |
| `dontneed_skipped` | shared pages left untouched for want of a frame. Expected 0; climbing means memory pressure is reaching this path |
| `dontneed_unaligned` | **still-open divergence**: Linux rejects a non-page-aligned `start` with `EINVAL`, this rounds it **down** (`dontneed_zero_range`), so the cleared range is a strict superset of Linux's and includes the caller's live head page. Has never read non-zero |

**Semantic gap (unchanged by the fix):** `MADV_DONTNEED` does not return pages to
the PMM, so a caller relying on it to shrink RSS will not see that effect. Doing
so would require teaching the eager path to register a region first.

**Regression coverage:** `madvshared` (in `verify_trim.py`'s `EXERCISES`, and
calibrated against real Linux arm64) and the boot test
`madvise_dontneed_spares_shared_frame`.

**`msync`** (`sys_msync`, `mem.rs:94`): always returns `0`, even for a range
with no writable `MAP_SHARED` file mapping (matches Linux: a no-op on a
clean/private range is a success, not an error).

**`membarrier`** (`membarrier_cmd`, `mem.rs:582`): `CMD_QUERY` returns a
capability bitmask (`0x18` = `PRIVATE_EXPEDITED | REGISTER_PRIVATE_EXPEDITED`).
`CMD_REGISTER_PRIVATE_EXPEDITED` is a no-op success. `CMD_PRIVATE_EXPEDITED`
issues a real `dsb ish` + `isb`. Any other cmd → `EINVAL`.

## Writable MAP_SHARED file-backed mappings (writeback)

This mechanism lives entirely in `mem.rs` — it is not covered in
`../memory.md`. Akuma has no unified page cache, so a file-backed mapping and
its backing file don't share storage automatically. When `flags & MAP_SHARED`
and `prot & PROT_WRITE` and the mapping is file-backed, `sys_mmap` forces the
**eager** (fully-resident) path and records `(tgid, base_va) →
SharedFileMapping { path, file_offset, len }` in `SHARED_FILE_MAPPINGS`
(`mem.rs:40`). Three syscall-visible flush points read that table back to the
file: `msync`, `munmap` (before the frames are freed), and process exit
(`flush_and_clear_shared_file_mappings`, called from the exit syscalls in
`proc.rs`). If the eager frame batch can't be satisfied even after
reclaiming, such a mapping fails with `ENOMEM` rather than silently
downgrading to `MAP_PRIVATE` semantics (a downgrade some earlier `go build`
traces logged — see Background).

## Feature notes

`mem.rs` is always compiled in (no `sc-*` gate; see
[`../syscalls.md`](../syscalls.md) "The `src/syscall/` split" table) — there
is no `ENOSYS`/porting gap in this family. The porting-relevant surprises are
the error-code quirks above, not missing syscalls.

## Background

- `archive/MEMORY_SYSCALL_STUB_FIXES.md` — the mremap `ENOMEM`-for-every-
  unmapped-probe bug (JSC's conservative GC issued 67 million `mremap`
  calls); fixed to return `EFAULT` for genuinely-unmapped ranges.
- `archive/FIX_MEMORY_MAPPING.md` — `MAP_POPULATE` and other silently-ignored
  flags, plus the fault-handler correctness/performance pass this syscall
  layer depends on.
- `archive/LLAMA_MMAP_OOM_KERNEL_ABORT.md` — the eager-mmap OOM → kernel abort
  chain that motivated the reclaim-and-retry-then-lazy-fallback path in
  `sys_mmap`.
- `archive/BUN_MEMORY_STUDY.md`, `archive/TCC_LOW_MEMORY.md` — per-binary mmap
  behavior studies.
