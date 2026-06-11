# llama.cpp `mmap=true` → ~4 GB allocation spike → kernel OOM abort (EC=0x3c)

**Status:** **FIXED (2026-06-11), including true mmap-larger-than-RAM.** Three
independent fixes landed:
1. **Kernel-heap growth runaway** (the actual cause of the original `brk #1` crash)
   — see "Real root cause" below.
2. **File-backed readahead reserve clamp** — a real fix for a separate OOM path.
3. **Clean file-page eviction** — the OS feature that makes an mmap *larger than
   physical RAM* actually work (see "mmap bigger than RAM" below).

With all three, `qwen3.5-0.8b-q4` (**532 MB**) loads, serves HTTP, and generates
correct output **in a 256 MB VM** (≈2× model:RAM) — slow (~17 s/token, disk-bound)
but stable, zero kernel crashes. At `MEMORY=4048` it runs at full speed. The model
fitting comfortably in RAM (e.g. ~1 GB for a 532 MB model) is the sweet spot:
eviction then only trims, no thrash.

## Userspace std::bad_alloc under pressure — FIXED (2026-06-11)

After the eviction + hang fixes, llama at 256 MB got further (prompt processed) then
aborted with `std::bad_alloc` (exit -6 SIGABRT — userspace, kernel survived, RAM fully
reclaimed). Cause: musl mmaps mid-size `new`/`malloc` allocations as small (≤16-page)
**eager** mmaps, which use the *critical* allocator `alloc_pages_zeroed` — that path
does NOT evict. Under pressure it returned ENOMEM → `new` got null → `std::bad_alloc`.

**Fix** (`src/syscall/mem.rs`): when the eager batch `alloc_pages_zeroed(pages)` fails,
`reclaim_clean_file_pages(pages + reserve)` and retry; if it still can't form the eager
batch, fall back to a **lazy (demand-paged) region** (`mmap_eager_to_lazy_fallback`) for
both anon and file-backed maps — a VA reservation that always succeeds and faults in via
the reclaim-aware path. Eager mmaps no longer hard-fail under pressure. (`sys_brk` is
already lazy via the fault path, so it needed no change.)

**Verified:** the exact request that aborted (`The capital of France is`) now returns
`"...The capital of France is **Paris**."` — a 532 MB model generating a correct answer
in a 256 MB VM (~15 s/token, disk-bound), 0 kernel crashes. The reclaim+retry succeeded
without even needing the lazy fallback.

## Intermittent kernel hang under concurrent mmaps — FIXED (2026-06-11)

Separate from the eviction work, a user hit an intermittent **kernel hang** (not a
crash — periodic output just stops) during llama's burst of ~1262 small graph-buffer
mmaps, with ~221 MB free (so NOT an OOM, and eviction never ran). Root cause: the
thread-group-owner `Process.mmap_regions` (and the fault path's eager fallback that
iterates it) is a plain `Vec` with **no synchronization**. llama's CLONE_VM threads
share one address space, so a concurrent `sys_mmap` push (one thread) and `.iter()`
read or `munmap` remove (another) can observe a half-completed `Vec` reallocation →
corruption → hang. Timing-dependent, hence intermittent (the same command served
fine on other runs).

**Fix:** added `Process::vm_lock` (`crates/akuma-exec/src/process/mod.rs`) and a
`vm_with_regions()` accessor that holds it with IRQs disabled for a **pure Vec op
only** (no alloc/map/free/IO/yield under the lock — frames to unmap/free are returned
and processed after release). All `mmap_regions` access (sys_mmap push, munmap/mremap
remove+split, the data-abort eager fallback, `record_mmap_region`/`remove_mmap_region`)
now goes through it. Validated: all VM self-tests pass (`munmap_teardown_conserves_pmm`,
`test_alloc_mmap_resolves_tgid`, `lazy_region_lookup_resolves_tgid`, etc.), and the
llama command serves at 256 MB. (Cold COW-fork copy paths read `mmap_regions` too but
aren't exercised by CLONE_VM threads; left unchanged.)

## mmap bigger than RAM — clean file-page eviction (2026-06-11)

Akuma demand-pages file-backed mmaps *in* but, before this, never paged them *out*,
so a model larger than RAM filled the PMM and OOM-killed the process. Added
**clean read-only file-page reclaim**:

- `UserAddressSpace::try_evict_ro_page(va)` (`crates/akuma-exec/src/mmu/mod.rs`):
  if `va` maps a VALID, **`AP_RO_ALL` (read-only)** page, clears the L3 PTE, flushes
  the TLB *before* freeing, and returns the frame (refcount-gated). Read-only ⇒
  content is authoritative in the backing file, so the next touch re-faults and
  re-reads. A CoW-dirtied page is `AP_RW_ALL` and is never evicted (no data loss).
- `process::reclaim_clean_file_pages(want)` (`crates/akuma-exec/src/process/children.rs`):
  snapshots the current address space's `LazySource::File` regions onto the stack
  (no heap alloc — it runs on the OOM path), sweeps them with a rotating cursor,
  evicts up to `want` read-only pages, frees via the runtime hook.
- Hooked into `pmm::alloc_page_zeroed_user()` (`src/pmm.rs`): under pressure, after
  heap reclaim, it evicts a batch (`USER_RECLAIM_BATCH = 512`) of clean file pages
  and retries before declaring OOM. Works for both file-fault and anon-fault
  pressure (evicting model weights to make room for anon compute buffers).

**Verified:** `/bin/mmap_file` touches every page of the 532 MB model in 256 MB and
**completes** (was a reserve SIGSEGV before). `llama-server --no-repack
--kv-cache-file /cache.img --mmap -fit off -b 64 -ub 64 -c 4096` loads + serves +
answers a chat request in 256 MB. To minimise the *non-evictable* (anon) footprint
so eviction can do its job: `--no-repack` (drops the ~495 MB CPU_REPACK buffer),
`--kv-cache-file <file>` (KV cache becomes file-backed, ~14 MB mmap vs ~519 MB anon),
`-fit off -b/-ub small` (shrinks the compute buffer). Limitation: it's a simple
rotating-cursor (FIFO-ish) replacement with no access-bit LRU, so a working set
larger than RAM thrashes — fine for "make it work," not yet optimised for speed.

## Original crash (kernel-heap runaway)

`llama-server` with the default `mmap=true` now loads the full model, serves HTTP,
and answers chat-completion requests at `MEMORY=4048` (extreme); the kernel heap
stays at ~7 MB (was ballooning to 3.88 GB) and RAM settles at the same ~986 MB
steady state as `--no-mmap`. See "Real root cause" below.

## Real root cause (2026-06-11) — kernel-heap growth runaway

Instrumenting the crash dump (per-process tracked-frame counts + per-site
demand-paging page counters + a heap-growth boundary log) showed the truth:

- The crashing process tracked only **15,948 user frames (~62 MB)**; demand paging
  mapped < 50 K pages total — the file/anon/mmap paths were innocent.
- The crash dump's own heap line was the tell: `Heap: 1.6 MB used / 3.88 GB total`.
  **The kernel heap had grown to 3.88 GB while only 1.6 MB was live.**
- The `[HEAP-GROW]` instrument showed every single growth was driven by a
  **262144-byte (256 KB / 64-page) allocation** that claimed exactly **64 pages**,
  with live usage stuck at 1 MB the whole time.

The bug: `handle_oom` claimed a span of *exactly* `needed` pages, but talc reserves
a few bytes of per-span metadata, so a 64-page span can't hold a 64-page
allocation. The request fell a few bytes short, talc re-invoked `handle_oom`, which
claimed another just-too-small span … forever, until the PMM was drained and the
next claim failed → `brk #1`. Any allocation whose size is an exact page multiple
≥ 256 KB hit it; llama's model-load issues recurring 256 KB reads, so it triggered
immediately. (The `[WATCHDOG] Time jump` was a *symptom* — the allocation burst
starved the vCPU — not a host sleep/wake artifact; reproduced under `caffeinate`.)

**Fix:** claim `HEAP_GROW_HEADROOM_PAGES` (2) pages above what the layout needs, so
the allocation fits after talc's overhead and the freed span is reused for the next
same-size request (`src/allocator.rs` — `handle_oom` + new `HEAP_GROW_HEADROOM_PAGES`
const). Regression test `test_heap_no_runaway_on_page_multiple_alloc`
(`src/process_tests.rs`) alloc/frees a 256 KB buffer 64× and asserts the heap total
barely moves (observed: **grew 0 bytes**; pre-fix would grow ~16 MB and, at scale,
all of RAM).

**Verified:** extreme, `MEMORY=4048`, default `mmap=true` — model loads, server
listens, a `/v1/chat/completions` request returns a valid response (~30 tok/s),
heap bounded at 7 MB, no abort.

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
