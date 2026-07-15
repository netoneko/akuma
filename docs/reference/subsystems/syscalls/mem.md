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

**`madvise`** (`sys_madvise`, `mem.rs:509`): never returns an error. All of
`MADV_WILLNEED`, `MADV_DONTNEED`, `MADV_FREE`, and any unrecognized advice
value return `0` unconditionally — advice is just that, and OOM during
`MADV_WILLNEED` pre-faulting is silently swallowed. **Semantic gap:**
`MADV_DONTNEED` here zeroes already-mapped pages in place
(`zero_mapped_page`) rather than unmapping and returning them to the PMM as
Linux does — a caller relying on `MADV_DONTNEED` to actually shrink RSS will
not see that effect.

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
