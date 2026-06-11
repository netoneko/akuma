# llama.cpp `mmap=true` → ~4 GB allocation spike → kernel OOM abort (EC=0x3c)

**Status:** **Partially addressed (2026-06-11). The llama `mmap=true` crash STILL
reproduces.** The file-backed readahead reserve gap is fixed and verified in
isolation, but it does NOT resolve this crash — re-running the exact repro below
(extreme, `MEMORY=4048`, fix included) still aborts with the identical `brk #1`
(EC=0x3c) signature, `PMM: 10/1036288 pages free`, ~580 MB process mmap vs ~4 GB
physical used. Only 24–45 file-region faults occur, so the model's file-backed
demand paging (the path that was hardened) is barely exercised; the ~3.4 GB spike
goes through another path that drains *below* the 16-page reserve. Reproduced both
with and without host sleep (`caffeinate`), so it is not a sleep/wake artifact (the
`[WATCHDOG] Time jump` line is a symptom of the allocation burst starving the vCPU,
not the cause). The real fix still needs the **open question** (instrument where the
3.4 GB comes from) and likely **Priority 2** (`MADV_DONTNEED` reclaim).

**Date:** 2026-06-10 (investigation), 2026-06-11 (Priority 1 fix)
**Profile / RAM:** extreme, `MEMORY=4048` (QEMU virt, HVF)
**Repro logs:** `4048mb_extreme_llama.cpp.log` (crash), `4048mb_extreme_llama.cpp_1.log` (`--no-mmap`, works)

## File-backed readahead reserve fix (2026-06-11) — necessary but NOT sufficient

This fix closes a real reserve-bypass in the file-readahead path and is verified in
isolation (see the `mmap_file` probe below), but **it does not stop the llama
`mmap=true` crash** — that exhausts memory through a different, still-unhardened path
(see Status). Documented here for completeness.

**Root cause (refined):** the file-backed demand-paging readahead path allocated
its batch with the *critical* PMM allocators (`alloc_pages_zeroed` /
`alloc_page_zeroed`) which bypass `USER_PAGE_RESERVE`, unlike the anonymous path
(`alloc_page_zeroed_user`). A model mmap larger than RAM therefore drained the PMM
to **0**, and the next *kernel-side* allocation (IRQ/scheduler/watchdog — note the
`[WATCHDOG] Time jump` line right before the dump) failed with no current process,
so `alloc_error_handler` `panic!`d → `brk #1` (EC=0x3c).

**Fix:** clamp file-backed readahead to `pmm::user_readahead_budget(free)` in both
the data-abort and instruction-abort handlers (`src/exceptions.rs`), and use the
reserve-aware `alloc_page_zeroed_user()` in the one-at-a-time fallback. The PMM now
floors at the 16-page reserve; the existing single-page fallback returns `None` and
the process is SIGSEGV'd. New helper `pmm::user_readahead_budget()`
(`src/pmm.rs`); boundary unit asserts + a live boot self-test
`test_mmap_file_oom_survives` (`src/process_tests.rs`, runs on profiles where the
suite is compiled in) backed by a new `/bin/mmap_file` probe.

**Verified (extreme profile, `MEMORY=256M`, model 507 MB > RAM):** `/bin/mmap_file`
on the model demand-pages until the PMM hits exactly `16 free pages`
(`[DA-DP] ... single-page fallback OOM, 16 free pages`), the process is killed
(`Process 3 (/bin/mmap_file) SIGSEGV after 13.70s`), **no `brk #1`/EC=0x3c**, and
the kernel stays up — SSH still responds and `free` shows the ~507 MB of
demand-paged frames fully reclaimed (259 MB free again, no leak).

## TL;DR

Running `llama-server` with the **default `mmap=true`** model loader exhausts all
~4 GB of physical RAM in ~2 seconds at the start of `load_tensors`, and the kernel
responds by executing `brk #1` (a Rust panic/abort, `EC=0x3c`) — taking the **whole
kernel down** instead of killing the offending process.

The **kernel must not crash because a userspace process asks for too much memory.**
An OOM condition has to degrade to a per-process `SIGSEGV`/kill, never a whole-kernel
abort. That is the primary bug to fix here; the memory-fitting behaviour that triggers
it is secondary (and largely a llama.cpp config matter).

Workaround that already works today: **`--no-mmap`** (stable at ~936 MB), or
`mmap=true` with `-fit off -c 2048`.

## Reproduction

```
llama-server --model /models/qwen2.5-0.5b-instruct-q4_k_m.gguf \
  --host 0.0.0.0 --port 11434 --chat-template chatml -c 32128
```

- `--no-mmap` added → **works**, steady-state `RAM: 3105/4048MB free` (~936 MB used),
  matching `free`:
  ```
  Mem:  4145152 KB total   936456 KB used   3208696 KB free
  ```
- default (`mmap=true`) → **kernel abort** (dump below).

## Evidence

### The model file is NOT what eats the RAM

The crash dump's own process accounting does not add up to the physical usage:

```
Process PID=13 '/bin/llama-server'
  Stack: 0x201fe06000-0x2020000000 (2024 KB)
  Heap:  brk grown=626688 bytes          ← 612 KB
  Mmap:  next=0x3443f000 ... used=608432128 bytes   ← 580 MB
PMM: 50/1036288 pages free (200 KB free) ← ~4 GB physical USED
```

- The process accounts for only **~580 MB** of address space, but **~4 GB of physical
  frames** are allocated. **~3.4 GB of physical memory is unattributable to the
  process's mappings** — i.e. a kernel-side over-allocation/leak on the `mmap=true`
  load path.
- Only **45** `[IA-DP] file region` faults occurred before the crash. Readahead is
  capped at 256 pages/fault (`READAHEAD_PAGES`, `src/exceptions.rs:2975`), so at most
  ~45 MB of the 467 MB model was ever paged in. **The demand-paged model data is not
  the source of the 4 GB.**

### It is a sudden spike, not a slow leak

Periodic `[Mem]` lines bracket the crash:

| Time  | RAM free        | Used    |
|-------|-----------------|---------|
| T164  | 3992/4048 MB    | ~56 MB  |
| T166  | 50 pages free   | ~4 GB   |

~3.9 GB consumed in ~2 seconds, right as `load_tensors` begins. This is why the
`--no-mmap` path (which uses a different allocation strategy) never gets near the
ceiling.

### The crash itself

```
[mmap] pid=13 fd=4 file=/models/qwen2.5-0.5b-instruct-q4_k_m.gguf off=0 len=0x1d4a2b60 = 0x16f9c000 (lazy-file, 37 regions)
[WATCHDOG] Time jump detected: 472ms (host sleep/wake)
[Exception] Sync from EL1: EC=0x3c, ISS=0x1
  ELR=0x40120698, FAR=0x13868000, SPSR=0x90000345
  Thread=10, TTBR0=0xd000040662000, TTBR1=0x401a4000
  Instruction at ELR: 0xd4200020          ← brk #1
  WARNING: Kernel accessing user-space address!
```

- `EC=0x3c` + instruction `0xd4200020` = `brk #1` = Rust panic → abort. `ELR=0x40120698`
  is the shared abort pad (cf. the documented `0x4012068c` OOM abort site).
- The `FAR=0x13868000` / "Kernel accessing user-space address" line is a **red herring**:
  `FAR` is stale/meaningless for a `BRK` exception; the generic exception printer just
  guesses. Do not chase that address.
- llama's last stderr line was `load_tensors: loading model tensors ... (mmap = true)`.

## Root cause analysis

### Confirmed bug #1 — OOM aborts the whole kernel instead of killing the process

Under genuine PMM exhaustion, some infallible kernel allocation panics (`brk #1`,
`EC=0x3c`). This is the same class of failure that
`docs`/[akuma_oom_kill_not_panic] addressed for the user demand-paging and
net-bounce paths, but it remains **open on whatever path allocated here**. A process
requesting more memory than exists must never panic the kernel — it must fail the
allocation and `SIGSEGV`/kill that process.

### Confirmed bug #2 — `MADV_DONTNEED` does not reclaim physical frames

`sys_madvise(MADV_DONTNEED)` (`src/syscall/mem.rs:404-417`) calls
`zero_mapped_page()` (`crates/akuma-exec/src/mmu/mod.rs:635-658`), which **zeroes the
page contents in place but leaves the PTE valid and the frame allocated**:

```rust
// zero_mapped_page: walks to the L3 PTE and does...
core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, 4096);
// PTE stays VALID, frame is never returned to the PMM.
```

Linux semantics: `MADV_DONTNEED` **frees** the backing frame and resets the page to
fault-on-next-touch (zero-fill for anon, re-read for file-backed). musl/llama
allocators use it to return freed memory to the OS while keeping the VA reserved.
Because Akuma keeps the frame, any allocator that reclaims via `MADV_DONTNEED` never
actually gives memory back. (`munmap` *does* free correctly —
`src/syscall/mem.rs:521-522,558-559` — so `madvise` is the outlier.)

This is a real reclaim bug and should be fixed regardless of this crash. It is a
plausible contributor to the unattributed ~3.4 GB, though it has not been definitively
tied to this specific spike (see "open question").

### Strong hypothesis — llama.cpp `-fit on` over-sizes buffers under `mmap=true`

llama.cpp's default `-fit on` sizes context/compute buffers to **free** device memory
(visible in the first run's stderr):

```
common_init_result: fitting params to device memory, for bugs during this step try -fit off ...
llama_params_fit: fitting params to free memory took 0.09 seconds
```

With `mmap=true` the 467 MB model is file-backed and does **not** count against
llama's memory budget, so the fit logic sees ~4 GB "free" and — amplified by the large
`-c 32128` context — sizes buffers to consume it. With `--no-mmap` the model occupies
the budget, fit stays modest, and everything fits in 4 GB. This cleanly explains the
`mmap`-vs-`--no-mmap` split and the 2-second spike.

Confidence: high on the mechanism, but llama's internal sizing was not directly
instrumented, so treat as hypothesis until confirmed.

### Open question — where exactly do the ~3.4 GB of frames come from?

The process VA only grew ~580 MB, yet ~4 GB of physical frames are live. The frames are
**not** in the process `mmap_regions` accounting and **not** the (capped) file-fault
readahead. Candidate paths to instrument:

- `MADV_WILLNEED` pre-fault batch (`src/syscall/mem.rs:356-402`) — allocates one frame
  per page in a lazy region; bounded by region size (~467 MB), not 4 GB.
- Eager `mmap` allocations (`src/syscall/mem.rs:175`) for large anonymous buffers.
- Frames allocated by the kernel on behalf of the process but never tracked in
  `mmap_regions` (a true leak) — combined with `MADV_DONTNEED` not reclaiming.
- The virtio-blk / ext2 read path bounce buffers (cf. the net 64 KB bounce-buffer note).

This needs empirical confirmation, not more static reading.

## Suggested fixes

### Priority 1 — the kernel must not crash on OOM (defensive, do first)

Make the allocation path that panicked here fallible and route OOM to a process kill.

- Find the infallible allocation reached during the spike (likely an `alloc`/`unwrap`
  in an eager mmap, a `Vec` growth in the read/readahead path, or a page-table frame
  alloc) and convert it to return `ENOMEM` / `SIGSEGV` the faulting process, the same
  way the user demand-paging and net-bounce paths were hardened.
- Add a PMM low-watermark reserve check so the *kernel's own* critical allocations
  (page tables, the abort/print path itself) can never be starved by a user process.
- Add a boot-suite self-test in `src/process_tests.rs` that spawns a process which
  requests far more memory than RAM and asserts the process dies with `SIGSEGV`/kill
  while the kernel stays up. (Per project rule: kernel changes need kernel tests.)

### Priority 2 — fix `MADV_DONTNEED` to actually reclaim

In `sys_madvise(MADV_DONTNEED)`:

- Unmap each page (clear the L3 PTE) and **free the frame** back to the PMM when this
  drops its last reference (mirror `munmap`'s `remove_user_frame` / `free_page` /
  `unmap_and_free_page_no_flush` logic in `src/syscall/mem.rs:517-562`).
- Reset the region so the next access re-faults: zero-fill for anonymous mappings,
  re-read from file for `LazySource::File` regions (so DONTNEED on a file mapping does
  not silently corrupt model data to zeros, which the current in-place zeroing does).
- Issue the batched TLB flush after clearing the range.
- Add a self-test: map, fault, `MADV_DONTNEED`, assert PMM free pages return to the
  pre-fault level, and assert the next read re-faults correctly.

### Priority 3 — make the trigger non-fatal at the source

- Default the model launch command to `--no-mmap` on Akuma (it works today), or
- Pass `-fit off` and a sane `-c` so llama does not size buffers to all free RAM.
  Document this in the model-serving runbook.

### Diagnostics to land alongside the fix

To close the "open question" and target Priority 1 at the right allocation:

- Add a per-process **tracked-frame counter** and dump it next to the PMM delta in the
  crash/`[Mem]` output, so the ~3.4 GB gap is attributable.
- Log every eager `[mmap]` ≥ N pages and every `MADV_WILLNEED`/`MADV_DONTNEED` call,
  then reproduce and watch the T164→T166 window to see which syscall path allocated
  the spike.

## Related

- `docs/COW_OPTIMIZATIONS.md` — lazy mmap / demand-paging design this builds on.
- Memory notes: net-bounce OOM kernel abort (fixed), "Kernel OOM = kill process, not
  panic" (fixed for other paths), qwen-0.8B OOM kernel abort, heap-growth backoff fix.
