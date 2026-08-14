# mem syscalls

mmap / munmap / brk / mremap / mprotect / madvise / msync / membarrier. Source:
`src/syscall/mem.rs`. For PMM, the kernel heap allocator, CoW fork, lazy
regions, the mmap bump allocator, MAP_FIXED, lazy anonymous mmap, and page
eviction, see [`../memory.md`](../memory.md) — this doc covers only the
syscall entry-point layer: argument validation, error codes, and quirks
visible at the syscall boundary.

> **Stability: C (active risk).** Inherits `memory.md`'s grade — mmap/munmap
> is the single highest-churn syscall file in the tree (22 commits, last
> 2026-06-19), because region-boundary bugs surface here first. The recurring
> lesson: **validate arguments before calling `lookup_process`** — an
> `EINVAL`/`EFAULT` check done first keeps a kernel-test caller (no current
> process) from getting `ESRCH` instead of the argument error Linux expects.

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
