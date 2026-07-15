# Memory

Current-state architecture for physical memory (PMM), the kernel heap, CoW
fork, and userspace address spaces. For debugging, see
[`../../runbooks/debug-memory-oom.md`](../../runbooks/debug-memory-oom.md).

> **Stability: C (active risk).** Highest-churn subsystem through 2026-06.
> Two items still OPEN: per-run kernel-heap creep; reclaim-after-OOM below
> ~5 MB. The recurring bug class is *region-boundary computed with a wrong
> constant* — verify any new region math against the invariants below.

## Memory layout

### Physical (QEMU `virt`)

| Range | Use |
|---|---|
| `0x00000000–0x3FFFFFFF` | Device MMIO (GIC dist `0x08000000`, GIC CPU `0x08010000`, UART `0x09000000`, fw_cfg `0x09020000`, VirtIO `0x0a000000`, GICv3 redist `0x080a0000`/`0x080b0000`) |
| `0x40000000` | RAM_BASE |
| `0x40100000` | Kernel load (ARM64 Image `text_offset`=1 MB) |
| `0x40200000` | DTB (QEMU-placed) |
| above `_kernel_phys_end` | 1 MB boot stack (linker-derived `STACK_BOTTOM`/`STACK_TOP`) + 2-page guard |

### Kernel regions (computed by `compute_memory_layout`)

- **Code+Stack** = `max(ram/16, MIN_CODE_AND_STACK_BYTES)`; extreme clamped via `MEM_CALC_CLAMP_MB`=4. **Invariant:** must cover `BOOT_STACK_TOP + 1 MB guard`.
- **Heap** = dynamic (seeded ~1 MB `size` / ~4 MB `release`, grows from PMM). The "fixed 16 MB heap" in `archive/MEMORY_LAYOUT.md` is **superseded**.
- **User pages** = remainder.
- **Device MMIO VAs:** L0[1] → `0x80_0000_0000+` (GIC), `0x80_0000_2000` (UART), `0x80_0000_4000` (VirtIO). **Invariant:** devices must NOT live in L0[0] (the bun 93 MB collision).

### Userspace VA (small binary)

```
0x00400000   code (.text/.data/.bss)
brk          heap (dynamic lazy region, grows up)
0x0a000000   VirtIO device pages (L2[80])
0x10000000   mmap region (grows up; jumps over kernel_va_end())
~0x3FFFF000  user stack (grows down; auto-sized, 128 KB at ≤256 MB)
0x40000000+  kernel RAM identity-mapped (EL1-only 2 MB blocks)
```

Large binaries (bun 93 MB) push brk into `0x05–0x09`; mmap placed above
`0x80000000`. `kernel_va_end()` = `round_up(ram_base+ram_size, 1GB)` (dynamic;
was hardcoded `0xC0000000`).

**Boot page tables:** 1 GB L1 blocks (L1[0] device, L1[1]/[2] RAM);
`mmu::extend_boot_ram_identity_map()` adds L1[3..] for RAM > 2 GB.

## PMM (physical memory manager)

`src/pmm.rs`. Bitmap allocator, 1 bit per 4 KB page. All ops wrapped in
`with_irqs_disabled` + `PMM.lock()`.

| API | Purpose |
|---|---|
| `alloc_page` / `alloc_page_zeroed` | Single page (zeroed via `phys_to_virt` + cache clean) |
| `alloc_page_zeroed_user` | Gated by `USER_PAGE_RESERVE`=16 (user pages can't drain the last kernel reserve) |
| `alloc_pages_contiguous_zeroed(count)` / `free_pages_contiguous` | Thread stacks |
| `free_page` | Returns `FreeOutcome { Freed, DoubleFree, OutOfRange }`; only `Freed` decrements `ALLOCATED_PAGES` |
| `cow_ref_inc(pa)` / `cow_ref_dec(pa) -> bool` | CoW refcounts (separate from bitmap). `dec` true ⇒ last ref, caller frees. |
| `stats()`, `double_free_count()` | `/Total/Allocated/Free`; the desync canary |

**Untracked-frame policy:** never free a frame you didn't track (leak is
recoverable; over-free crashes the kernel).

## Kernel heap allocator

`src/allocator.rs`. **Not a slab** — a dynamic linked-list/talc allocator:
`talc::Talc<PmmOomHandler>` behind a `Spinlock`.

- **Growth:** seeded with a small bootstrap arena; `PmmOomHandler::handle_oom`
  claims contiguous pages from PMM. `HEAP_GROW_PAGES`=64 amortised; under
  pressure `heap_grow_backoff` halves `n` toward `needed` (kills the
  fragmentation abort).
- **Reclaim:** `reclaim_to_pmm()` returns wholly-free spans;
  `claimed_span_report()` exposes live/pinned spans.
- **Watermark:** `is_memory_low()` (free heap < 2 MB `HEAP_LOW_WATERMARK`) —
  circuit breaker at fork/clone/spawn/SSH accept.
- **Failure path:** `#[alloc_error_handler]` (`src/allocator.rs:499`) prints
  `[OOM] allocation of N bytes failed`, calls `return_to_kernel(-12)` if
  in-process, else `panic!`.
- **`mark_pmm_ready()`** flips from talc-only to page-backed growth after PMM init.

## CoW fork

- **Share path** (`fork_process`, `COW_FORK_ENABLED`): `cow_share_range` →
  per page `cow_ref_inc(pa)` + child `map_page(RO)` + `track_user_frame`; then
  parent `demote_range_to_ro`; `flush_tlb_asid(0)` (all ASIDs — sibling threads
  share L0 with different ASIDs).
- **Fault path** (write to RO page, `exceptions.rs`): alloc copy,
  `track_user_frame(new)`, `remove_user_frame(old)`, `cow_ref_dec(old)`.
- **Data structure:** `user_frames: BTreeMap<PA, u32>` (refcount = in-AS mapping
  count; was `Vec` → O(n²) munmap, fixed to O(log n)).

### CoW invariants (the desync surface)

1. `track_user_frame` count must equal `cow_ref_inc` count (1:1) — ~30
   hand-maintained call sites. Any new mapping of a physical page must do both.
2. `remove_user_frame` is `#[must_use] -> bool` (true ⇒ last ref, caller owns
   the free). munmap loops free **only when it returns true**.
3. `Drop` frees each distinct PA **once** (count is mapping refcount, not alloc
   count).
4. Untracked-frame policy: **don't free** (leak is recoverable; over-free
   crashes kernel).
5. **vfork fast-path** (`VFORK_FASTPATH_ENABLED`, `CLONE_VFORK|CLONE_VM`):
   `new_shared(parent_l0)` (no copy, no demote); `replace_image` drops shared
   view on exec → parent takes zero CoW faults.

## Userspace memory model

- **Page tables:** per-process TTBR0, AArch64 4-level / 4 KB granule.
  `add_kernel_mappings` identity-maps full RAM as EL1-only 2 MB blocks (so
  `phys_to_virt` works during syscalls); device MMIO via shared L0[1].
- **Demand paging:** "lazy regions" (`push_lazy_region` →
  `ensure_user_page_mapped`) for heap, large mmap, stack, large ELF segments
  (`DeferredLazySegment` via `from_elf_path`).
- **mmap:** bump from `0x10000000`, first-fit into `free_regions` on munmap;
  jumps over `kernel_va_end()`; MAP_FIXED may shatter a 2 MB block into L3.
- **Lazy anonymous mmap:** `MAP_PRIVATE` > `MMAP_EAGER_MAX_PAGES`=16 → lazy
  region, zero-on-demand.
- **Stack:** top ~`0x3FFFF000`; auto-sized (128 KB at ≤256 MB by default).
- **Process info page:** `PROCESS_INFO_ADDR`=0x1000 (pid; read only with user
  TTBR0 active).
- **libakuma allocator:** default page-based (mmap per alloc) **or**
  `chunked-allocator` feature (Talc in 64 KB chunks). Deferred-free queue for
  realloc safety.
- **Eviction:** `try_evict_ro_page` + `reclaim_clean_file_pages` evict clean
  `AP_RO_ALL` file-backed pages under pressure (lets models > RAM run).

## Background

- `archive/MEMORY_LAYOUT.md` (partly superseded on heap sizing).
- `archive/USERSPACE_MEMORY_MODEL.md`, `archive/COW_OPTIMIZATIONS.md`.
- `archive/LOW_MEMORY_ENVIRONMENT.md` — the densest source; extreme hardening.
- `archive/HEAP_AND_MEMORY_IMPROVEMENTS.md` — watermark + admission control.
