# Running Akuma in Low-Memory Environments

How Akuma sizes its memory regions, what actually limits the **minimum bootable
RAM**, and the heuristics that let it scale from tiny VMs up to multi-GB boxes.

Set RAM with the `MEMORY` env var: `MEMORY=64M cargo run --release`.

## TL;DR

Verified with `scripts/test_memory_split.py` + the small-RAM sweeps in `logs/`
(tcc compiling `/akuma-playground/hello.c`):

**Measured `size`-profile sweep (June 2026, 883 KB binary, `tcc /akuma-playground/hello.c -o /tmp/hello`).** The action plan below (items 1–5) has since landed; these are the **post-fix** numbers:

| RAM | boots to SSH? | SSH usable? | runs tcc `hello.c`? | code+stack / heap / user / thread-limit | notes |
|---|---|---|---|---|---|
| 48 MB | yes | yes | **yes** | 5 / 6 / 37 / 64 | — |
| 32 MB | yes | yes | **yes** | 5 / 4 / 23 / 64 | was **no** (anon alloc OOM) before the fixes |
| 24 MB | yes | yes | **yes** | 5 / 4 / 15 / 40 | was **no** (ELF load OOM) before |
| 16 MB | yes | yes | **yes** | 5 / 4 / 7 / 14 | was **no** (SSH rejected, "memory low") before |
| 12 MB | yes | yes | **no** | 3 / 3 / 4 / 14 | OOM spawning the tcc process — new tcc floor is just above here |
| **8 MB** | **yes** | **yes** | **no** | 5 / 1 / 2 / 14 | **first time 8 MB boots to SSH** (June 2026, PmmOomHandler); 716 KB free, tcc ELF alone is 723 KB |

So on the `size` profile after the fixes: **boot/usable-SSH floor ≤ 8 MB, tcc floor
16 MB** — down from a 48 MB tcc floor. For reference, the
**pre-fix** floors were boot 16 MB / usable-SSH 24 MB / tcc 48 MB, with `code+stack`
a flat **11 MB** at every size. The 8 MB SSH floor is new as of the `PmmOomHandler`
change (heap seed 4 MB → 1 MB, grows on demand from PMM): previously SSH was
rejected at 8 MB with "kernel memory low" because `is_memory_low()` compared against
a 2 MB watermark on the 1 MB seeded slab. (`tcc -run` is separately broken at *all* sizes —
`runmain.o not found`, a TCC runtime-install issue, not RAM; use `tcc … -o out` then
exec `out`.) On the `release` profile tcc is verified at ≥ 256 MB via
`scripts/test_memory_split.py`.

What moved the floor: four over-provisioned "≥ 256 MB" reservations were cut on the
`size` profile — the 8 MB **eager** user stack → 128 KB (item 1), the 8 MB heap floor
→ 4 MB (item 5), 128 KB → 64 KB per-user-thread kernel stacks (item 3), and most
decisively the **`code+stack` region 11 MB → 5 MB** by removing the stale 8 MB
boot-stack gap (item 4). `code+stack` is now a flat ~5 MB instead of 11 MB. The
`PmmOomHandler` then dropped the heap seed to 1 MB (was 4 MB), unlocking SSH at 8 MB.

Two things were originally hardcoded for "≥ 256 MB machines" and made to scale (the
earlier change that got the kernel *booting* across the range):

1. **Kernel heap** — was a flat 64 MB floor below 512 MB RAM (left 0 user pages
   below ~72 MB → no boot). Now `compute_heap_size()` scales it down under 256 MB.
2. **Thread-stack pool** — the scheduler eagerly allocates a stack per thread slot
   for all `MAX_THREADS` (64) from **PMM**: 7 system × 256 KB + 56 user × 128 KB =
   **8.96 MB**. On a small machine that pool is the dominant fixed cost and the
   real boot floor. Now the number of slots that get stacks scales with RAM
   (`compute_thread_limit()`).

## Boot-stack bug — stale hardcoded address corrupted the heap (FIXED 2026-06-03)

**Status: FIXED.** Before the fix, `release` failed to boot at 16/24/32/64 MB; after
the fix it boots at all of them (verified 16/24/32, tcc `hello.c` runs at 32 MB) and
`size` is unregressed. **Release boot floor: 128 MB → ≤ 16 MB.**

**Matrix sweep that pinned it (both profiles × {16,24,32,64,128,256,1024,4096 MB},
shared apk-seeded disk):** the `size` profile booted + ran tcc at **every** size down
to 16 MB; the `release` profile failed to boot at **16 / 24 / 32 / 64 MB** (all four
the identical crash below) and booted only at **≥ 128 MB** — and the four release
failures were all this one bug.

**Symptom.** On the **`release`** profile, low-RAM boots crash **during
"Initializing threading…"** with a data abort:
`[Exception] Sync from EL1: EC=0x25, ISS=0x4 … FAR=0xdeadbeefcafebace`, then the
scheduler spins on `yield_now with IRQs masked tid=0` (hung, never reaches SSH).
`FAR` is `STACK_CANARY (0xDEAD_BEEF_CAFE_BABE) + 0x10`, and `ELR` lands inside
`talc::Talc::malloc` — i.e. the heap allocator dereferenced a free-list pointer that
held the **stack-canary** value. (Earlier `size`-profile sweeps that reported tcc at
16 MB likely hit the same corruption nondeterministically; the matrix sweep is
re-measuring both profiles to confirm exactly which cells crash.)

**Root cause (theory).** `crates/akuma-exec/src/threading/mod.rs:1110-1111` still
hardcodes the **old** boot-stack location:

```rust
let _boot_stack_top = 0x40800000u64; // STACK_TOP from boot.rs
let boot_stack_base = 0x40700000usize; // STACK_TOP - STACK_SIZE
```

…and line 1138-1139 does `init_stack_canary(boot_stack_base)` → writes 8 ×
`0xDEAD_BEEF_CAFE_BABE` at `0x40700000`. But the boot stack was **relocated** when the
profile-aware image layout landed (`boot.rs`/`build.rs`/`linker.ld`): it now lives at
`0x40500000–0x40600000` on `release` and `0x40300000–0x40400000` on `size`. This
constant in the threading crate was **never updated** — and `ExecConfig` doesn't pass
the boot-stack address in, so the crate has no way to know the real one.

**Why it's RAM-dependent (the "layout" part).** `heap_start = ram_base +
code_and_stack`. With the item-4 shrink (`MIN_CODE_AND_STACK = 4 MB`, `stack_cover ≈
7 MB` on release), for any RAM where `ram/16 < ~7 MB` (**≤ 64 MB**) the heap starts at
exactly **`0x40700000`** — the release@32 log shows `Heap: 8 MB (0x40700000 -
0x40f00000)`, heap byte 0. So the stale canary write stamps directly onto **talc's
arena header**; the next `malloc` walks the corrupted free list and faults. At **≥ 128
MB**, `ram/16 ≥ 8 MB` pushes `heap_start` above `0x40700000`, so the stray write lands
in dead space inside the oversized code+stack reservation and boot survives — which is
exactly the observed floor (boots at 128/256/1024/4096, crashes at 16/24/32/64).

**Why `size` dodges it.** The `size` profile's `code+stack` is **5 MB** (smaller
binary → `IMAGE_SIZE = 1 MB` → `stack_cover ≈ 4 MB`), so its `heap_start` is
`0x40500000`. The stale `0x40700000` write therefore lands **2 MB into** the heap —
past talc's arena header that `malloc` walks first — so it corrupts a less-critical
region and boot happens to survive. That's luck, not correctness: the same stale write
is still firing on `size` too, just not onto the byte that crashes. The fix removes the
hazard from both profiles.

**The fix (applied).** Stop hardcoding the boot-stack address in the threading crate:

- `ExecConfig` gained `boot_stack_base` / `boot_stack_top` fields
  (`crates/akuma-exec/src/runtime.rs`).
- `main.rs` populates them from the per-profile `STACK_BOTTOM` / `BOOT_STACK_TOP`
  consts it already computes, so the crate gets the *real* address.
- `threading::init` (`crates/akuma-exec/src/threading/mod.rs` ~1110) uses
  `config().boot_stack_base/top` instead of `0x40700000`/`0x40800000`, so
  `init_stack_canary` writes into the actual boot stack (code+stack region), never the
  heap.
- `exceptions.rs::init_exception_stack` had the **same** stale `0x40800000` for the
  boot thread's early exception stack; it's now profile-aware
  (size `0x40400000` / release `0x40600000` / firecracker `0x80800000`), so an early
  exception can't scribble into the heap either.

**Verification.** `release` now boots to SSH at 16/24/32 MB (was: hang at all of them);
`tcc /akuma-playground/hello.c` compiles + runs at release@32 → "Hello, Akuma!";
`size` still boots at 16 MB (no regression). Release boot floor dropped 128 MB → ≤ 16
MB.

## Per-RAM memory statistics (June 2026)

Computed from the live heuristics in `src/main.rs` (`compute_heap_size`, `compute_thread_limit`)
and `src/config.rs` (`USER_THREAD_STACK_SIZE`, `USER_STACK_SIZE_OVERRIDE`).
Layout constants: size profile `stack_cover = 5 MB`; release profile `stack_cover = 7 MB`.
Thread pool comes from user pages (PMM), not the heap.

**size profile** — 883 KB binary, `IMAGE_SIZE` 1 MB, `USER_THREAD_STACK_SIZE` 64 KB, user-stack auto-scales (≤ 256 MB → 128 KB). Heap seed is now 1 MB (grows on demand via `PmmOomHandler`):

| RAM | code+stack | heap seed | user pages | threads | stack pool | free for procs | % of RAM | user stack/proc | notes |
|-----|-----------|------|-----------|---------|-----------|---------------|---------|----------------|-------|
| 8 MB | 5 MB | 1 MB | 2 MB | 14 | 1.28 MB | 0.72 MB | 9% | 128 KB | **SSH works** (June 2026); 716 KB free; tcc ELF 723 KB → OOM |
| 16 MB | 5 MB | 1 MB | 10 MB | 14 | 1.28 MB | 8.7 MB | 54% | 128 KB | tcc: yes |
| 24 MB | 5 MB | 1 MB | 18 MB | 40 | 3.75 MB | 14.3 MB | 60% | 128 KB | meow+tcc: yes |
| 32 MB | 5 MB | 1 MB | 26 MB | 64 | 5.25 MB | 20.8 MB | 65% | 128 KB | comfortable |
| 128 MB | 8 MB | 16 MB | 104 MB | 64 | 5.25 MB | 98.8 MB | 77% | 128 KB | — |
| 256 MB | 16 MB | 64 MB | 176 MB | 64 | 5.25 MB | 170.8 MB | 67% | 128 KB | heap jump at 256 MB threshold |
| 2048 MB | 128 MB | 256 MB | 1664 MB | 64 | 5.25 MB | 1659 MB | 81% | 1 MB | — |
| 4096 MB | 256 MB | 256 MB | 3584 MB | 64 | 5.25 MB | 3579 MB | 87% | 2 MB | — |

Note: for ≥ 16 MB the heap grows into the user-page pool via `PmmOomHandler` as needed,
so "heap seed" is the initial reservation; effective heap ceiling is whatever PMM has free.

**release profile** — 2833 KB binary, `IMAGE_SIZE` 3 MB, `USER_THREAD_STACK_SIZE` 128 KB, user-stack auto-scales (`USER_STACK_SIZE_OVERRIDE = 0` as of June 2026):

| RAM | code+stack | heap | user pages | threads | stack pool | free for procs | % of RAM | user stack/proc | notes |
|-----|-----------|------|-----------|---------|-----------|---------------|---------|----------------|-------|
| 16 MB | 7 MB | 5 MB | 4 MB | 14 | 2.5 MB | 1.5 MB | 9% | 128 KB | very tight |
| 24 MB | 7 MB | 8 MB | 9 MB | 14 | 2.5 MB | 6.5 MB | 27% | 128 KB | — |
| 32 MB | 7 MB | 8 MB | 17 MB | 28 | 4.25 MB | 12.75 MB | 40% | 128 KB | meow+tcc: fits |
| 128 MB | 8 MB | 16 MB | 104 MB | 64 | 8.75 MB | 95.3 MB | 74% | 128 KB | — |
| 256 MB | 16 MB | 64 MB | 176 MB | 64 | 8.75 MB | 167.3 MB | 65% | 128 KB | heap jump at 256 MB threshold |
| 2048 MB | 128 MB | 256 MB | 1664 MB | 64 | 8.75 MB | 1655 MB | 81% | 1 MB | — |
| 4096 MB | 256 MB | 256 MB | 3584 MB | 64 | 8.75 MB | 3575 MB | 87% | 8 MB | auto-scaled max |

Stack pool formula: `7 × 256 KB + (threads − 8) × USER_THREAD_STACK_SIZE`.
Free-for-procs = user pages − stack pool (boot-time static); each process load = ELF mapped pages + user stack + runtime heap drawn from this pool at runtime.

### meow+tcc sweep results (June 2026, `SYSTEM_THREAD_STACK_SIZE` tuning)

`meow -m qwen3-yolo:latest -c "compile /akuma-playground/hello.c with /usr/bin/tcc -B /usr/lib/tcc -o /tmp/hello_c, run /tmp/hello_c"`.
Prerequisites on disk: `apk add tcc musl-dev tcc-libs tcc-libs-static` (installs `/usr/bin/tcc`).

**Test setup notes:** disk must be clean before each sweep — multiple `pkill -9` kills corrupt
ext2, leaving stale `/tmp/hello_c` that masks compile failures. Recreate with
`scripts/create_disk.sh && scripts/populate_disk.sh`, then `apk add tcc musl-dev tcc-libs tcc-libs-static`
at a high-memory boot before sweeping down. The sweep prompt must use `/usr/bin/tcc`
(apk-installed, has headers) not `/bin/tcc` (bootstrap binary, missing `tccdefs.h`).

**release profile, `SYSTEM_THREAD_STACK_SIZE = 256 KB` (original):**

| RAM | meow+tcc | notes |
|-----|---------|-------|
| 32 MB | PASS | 12/32 MB RAM free during run |
| 24 MB | PASS | — |
| 16 MB | FAIL | `RAM: 0/16MB free` — meow exhausts all 1.5 MB free-for-procs; no room to spawn tcc |
| 12 MB | — | not tested |

**release profile, `SYSTEM_THREAD_STACK_SIZE = 64 KB` (−1.3 MB pool, June 2026):**

| RAM | meow+tcc | free-for-procs | heap peak | notes |
|-----|---------|---------------|-----------|-------|
| 32 MB | PASS | 16.2 MB | 2.8 MB | — |
| 24 MB | PASS | 8.2 MB | 2.8 MB | — |
| 16 MB | **PASS** | 2.8 MB | 2.8 MB | unlocked by 64 KB system stacks |
| 12 MB | FAIL | 2.8 MB | 1 MB (cap) | kernel heap collapses to 1 MB (`code+stack=7, cap=1`); meow peaks at 2.8 MB → OOM |

12 MB is a layout floor for `release`: `code+stack=7 MB` (driven by the 3 MB binary + `stack_cover`) leaves only 1 MB for heap — below meow's 2.8 MB kernel-heap peak. Not fixable by stack tuning alone; needs a smaller binary (→ `size` profile) or a different allocator.

**size profile, `SYSTEM_THREAD_STACK_SIZE = 128 KB` (64 KB caused kernel stack overflow at -Oz):**

| RAM | meow+tcc | RAM free during run | Heap free | notes |
|-----|---------|-------------------|-----------|-------|
| 16 MB | needs clean disk | 4/16 MB | 1/4 MB | tcc compiles directly; stale `/tmp` from ext2 corruption masked meow results |
| 12 MB | needs clean disk | 2/12 MB | 0/3 MB | heap nearly exhausted during meow LLM call |
| 8 MB | FAIL (boot) | — | 1 MB heap (cap) | SSH rejected: memory low |

Size profile at 8 MB: `code+stack=5 MB`, heap collapses to 1 MB (`cap=8−5−4`), SSH rejects
the connection. The `size` floor with current Talc allocator is theoretically **12 MB**
(4 MB user pages, 3 MB heap — barely enough for meow's 2.4 MB heap peak + tcc spawn),
but needs a clean-disk re-run to confirm.

**Blocked by Talc's fixed reservation below 12 MB.** At 8 MB the heap cap formula yields
1 MB — not enough for SSH + meow. A dynamic allocator that draws from the same physical
pool as user pages (instead of a fixed upfront reservation) would let heap and processes
share the remaining RAM, potentially pushing the floor to 6–8 MB.

### Gains from `USER_STACK_SIZE_OVERRIDE` 8 MB → 0 (auto-scale)

The `% of RAM` column above is **identical before and after** — the boot-time pool doesn't change.
What changes is the per-process runtime cost: 8 MB + ~0.5 MB ELF = **~8.5 MB/proc before** vs
128 KB + ~0.5 MB ELF = **~0.6 MB/proc after** (at ≤ 256 MB RAM). Gains in max concurrent
user processes (capped at available user thread slots = `thread_limit − 8`):

| RAM | before (8 MB stacks) | after (128 KB stacks) | gain |
|-----|---------------------|-----------------------|------|
| 16 MB | **0** (8.5 MB > 1.5 MB free) | **2** | — → 2 |
| 24 MB | **0** (8.5 MB > 6.5 MB free) | **6** (slot-capped) | — → 6 |
| 32 MB | **1** | **20** (slot-capped) | 1 → 20 |
| 128 MB | **11** | **56** (slot-capped) | 11 → 56 |
| 256 MB | **19** | **56** (slot-capped) | 19 → 56 |
| 2048 MB | **56** (slot-capped) | **56** (slot-capped) | unchanged |
| 4096 MB | **56** (slot-capped) | **56** (slot-capped) | unchanged |

**Rustc regression note:** `rustc hello.rs` was verified working on `release` at 2048 MB prior
to the `USER_STACK_SIZE_OVERRIDE = 0` change. Re-verify this after the change — rustc's codegen
threads are stack-hungry and may need a larger override than the 1 MB auto-scaled value at 2 GB.
If it regresses, try `USER_STACK_SIZE_OVERRIDE = 2 * 1024 * 1024` before reaching for 8 MB.

Below 256 MB the gains are dramatic because 8 MB stacks were consuming the entire free pool
for 1–2 processes. Above 256 MB the thread-slot limit (56 user slots) is the binding
constraint either way. The override was a debugging artefact from crush/bun work — it had
no benefit for any workload that doesn't actually touch 8 MB of stack depth.

### meow → Qwen → tcc hello.c — minimum viable RAM

Binary sizes: `meow` 403 KB, `tcc` 589 KB, `dash` ~100 KB, compiled `hello` 71 KB.
The pipeline is sequential (dash forks meow; waits; then forks tcc; waits) so the peak
concurrent load is dash + one child, never meow + tcc simultaneously.

**size profile (128 KB user stacks, all RAM tiers):**

| process | ELF | stack | est. runtime heap | total |
|---------|-----|-------|------------------|-------|
| dash (shell) | ~100 KB | 128 KB | ~100 KB | ~328 KB |
| meow (HTTP + JSON) | 403 KB | 128 KB | ~512 KB | ~1 MB |
| tcc (compile hello.c) | 589 KB | 128 KB | ~512 KB | ~1.2 MB |
| hello (run output) | 71 KB | 128 KB | minimal | ~200 KB |

Peak concurrent: dash + meow ≈ 1.3 MB; all fit comfortably within the 4.9 MB free at 16 MB.
**Minimum for meow+tcc on `size` profile: 16 MB** (same floor as tcc alone).

**release profile (8 MB eager user stacks):**

| process | ELF | stack (eager) | est. total |
|---------|-----|--------------|-----------|
| dash | ~100 KB | 8 MB | ~8.1 MB |
| meow | 403 KB | 8 MB | ~8.5 MB |
| tcc | 589 KB | 8 MB | ~8.6 MB |

Peak concurrent: dash + meow = ~16.6 MB user pages needed.
At 32 MB release only 12.75 MB is free for processes → OOM loading meow alongside dash.
At 64 MB release (not in table): user pages ≈ 49 MB, free ≈ 40 MB → fits 4 concurrent procs.
**Minimum for meow+tcc on `release` profile: 64 MB** (32 MB is borderline; avoid it).

## apk command memory floor (June 2026)

`apk search` and `apk add busybox` tested at each RAM tier with `SNAPSHOT=1` (disk
writes discarded) so the disk is never modified between runs. The bottleneck is
**apk's own working set** — the package manager faults in ~48 MB of anonymous
user pages during a run (TLS, zlib, resolver, package index fetch), which dwarfs
the per-profile kernel overhead. Consequently, both profiles share the same floor.

**Measured sweep (June 2026, `scripts/apk_memory_sweep.py`, both profiles):**

| RAM | profile | boots? | `apk search busybox` | `apk add busybox` | notes |
|-----|---------|--------|---------------------|-------------------|-------|
| ≤ 64 MB | release | yes | **no** | **no** | `/bin/apk` SIGSEGV — PMM exhausted during ELF mapping |
| 72 MB | release | yes | **no** | **no** | PMM still exhausted |
| **80 MB** | release | yes | **yes** (2 s) | **yes** (63 s) | floor — 7 MB RAM headroom |
| ≥ 96 MB | release | yes | yes (2 s) | yes (63 s) | comfortable |
| ≤ 64 MB | size | yes | **no** | **no** | same SIGSEGV; smaller kernel saves < 2 MB vs apk's 48 MB need |
| 72 MB | size | yes | **no** | **no** | PMM still exhausted |
| **80 MB** | size | yes | **yes** (2 s) | **yes** (910 s) | floor — only 7 MB RAM free; network stack starved → 14-min wait |
| ≥ 96 MB | size | yes | yes (3 s) | yes (64 s) | comfortable |

**Why 80 MB for both profiles.** At 80 MB the user-page pool is just large enough
for apk's ~48 MB working set plus the thread stack pool. The `size` profile's
5 MB smaller kernel overhead (`code+stack 5 MB vs 7 MB`) does not shift the
threshold — apk's demand dominates. At exactly 80 MB (`RAM: 7/80 MB free`) the
`size` profile squeezes through, but with so little headroom that apk's
`pselect6` network wait balloons to 14 minutes (vs 63 s at ≥96 MB); use **96 MB
as the practical floor for reliable apk use on either profile**.

**Root cause: apk's PMM demand.** The SIGSEGV failures at ≤ 72 MB come from PMM
exhaustion (`pmm=0free` in kernel stats): apk maps memory via many `mmap` calls
(TLS, heap, package-index buffers), each demand-paged; when the PMM runs dry the
next page fault has no backing page → SIGSEGV. This is the same mechanism as an
OOM kill but without a dedicated OOM handler — the kernel just signals the process.

**Comparison with tcc.**

| workload | release floor | size floor |
|----------|--------------|-----------|
| boot + SSH | 16 MB | 12 MB |
| `tcc hello.c -o /tmp/h && /tmp/h` | 32 MB | 16 MB |
| `apk search busybox` | 80 MB | 80 MB |
| `apk add busybox` | 80 MB | 80 MB (reliable: 96 MB) |

## The three regions

`src/main.rs::kernel_main` splits detected RAM into:

```
[ Code + Stack ] [ Heap ] [ User pages (PMM) ]
   ~5 MB const   see below   remainder
```

- **Code + Stack** — kernel binary + boot stack, now placed **adjacent** to the binary
  (`STACK_BOTTOM = ram_base + IMAGE_SIZE`, `BOOT_STACK_TOP = STACK_BOTTOM + 1 MB`). With
  `MIN_CODE_AND_STACK = 4 MB` this works out to a flat **~5 MB** at every RAM size. (It
  was `max(ram/16, 8 MB)` with the boot stack hardcoded 8 MB above the base — an 11 MB
  region — until action plan item 4 removed that gap.)
- **Heap** — kernel data structures (`alloc`). Boot uses only ~2.2 MB; it grows
  under load (VFS, process tables). Sized by `compute_heap_size()`.
- **User pages** — the PMM free pool. Backs **both** user process memory **and the
  thread-stack pool** (stacks come from `alloc_pages_contiguous_zeroed`, *not* the
  heap — this is the key subtlety).

## Heap heuristic — `compute_heap_size(ram_size, code_and_stack)`

- `config::KERNEL_HEAP_SIZE_MB != 0` → fixed override (in MiB).
- **RAM ≥ 256 MB**: `clamp(ram/8, 64 MB, 256 MB)` — unchanged; preserves the
  proven default and headroom for go/bun/rustc kernel metadata.
- **RAM < 256 MB**: `clamp(ram/8, SMALL_FLOOR, ram − code_stack − MIN_USER)` — scaling
  by `ram/8`, never eating the last few MB of user pages. `SMALL_FLOOR` is **8 MB** on
  `release` but **4 MB** on the `size` profile (item 5): the kernel boots on ~2.2 MB, so
  on a 24 MB box the lower floor hands the freed 4 MB straight to user pages (5 → 15 MB),
  which is what let tcc's ELF load fit.

## Thread-pool heuristic — `compute_thread_limit(user_pages_size)`

`MAX_THREADS` (64) stays a compile-time constant (it sizes per-thread atomic
arrays — cheap). What scales is **how many slots get a stack allocated**, a
runtime `thread_limit ≤ MAX_THREADS` set before `threading::init`:

- The `reserved` (8) low slots are kernel/system threads (idle, network, SSH,
  async executor) and always get stacks (`7 × 256 KB = 1.75 MB` pool minimum).
- User-process slots `[reserved, thread_limit)` get `128 KB` stacks on `release`, but
  **64 KB** on the `size` profile (item 3) — halving the per-slot PMM cost so more slots
  fit the same budget.
- `thread_limit` is chosen so the stack pool uses **at most ~1/4 of user pages**,
  leaving the rest for actual processes — with a floor of `reserved + 6` (item 2) so a
  shell + SSH session + tcc + a subprocess can coexist. (Earlier floors were too low:
  `reserved + 2` left only 2 user slots at 16–24 MB; an even earlier 1/2 *budget* was
  too greedy — on a 32 MB box it allocated 58 threads / 8 MB of stacks and the first
  process ELF load then OOM'd.)

`config::THREAD_LIMIT_OVERRIDE` (0 = auto) pins it for testing.

Stack pool for `N` total threads: `1.75 MB + (N − 8) × 128 KB`. Examples:

| threads N | pool |
|---|---|
| 64 | 8.96 MB |
| 32 | 4.75 MB |
| 24 | 3.75 MB |
| 16 | 2.75 MB |
| 12 | 2.25 MB |

## Why the old build died below ~128 MB

- Flat 64 MB heap floor: at 64 MB RAM the heap consumed everything →
  `User pages: 0 MB` → PMM empty → boot can't create any process.
- `verify_stack_memory()` compared the stack pool against the **heap** size, but
  stacks are allocated from **PMM**. So with a big heap the check passed, then the
  PMM stack allocation failed (`"Failed to allocate thread stack from PMM"`); with
  a small heap the check itself false-panicked (`"Stack memory exceeds heap"`).
  It now validates against PMM free pages for the scaled `thread_limit`.

## Boot self-tests on tiny machines

The boot self-test suite includes resource-heavy tests that spawn several
parallel processes/threads (`spawn_multiple`, `spawn_and_yield`,
`spawn_cooperative`, `yield_cycle`, `mixed_cooperative_preemptible`,
`neon_regs_across_preemption`, `fpcr_fpsr_across_yield`,
`fp_arithmetic_across_preemption`, `parallel_processes`). With a small thread
pool and few user pages these can't run and would halt the boot before SSH.

Below `config::LOW_MEM_TEST_SKIP_MB` (default **32 MB**) of detected RAM these are
skipped (`run_test_heavy!`); the core correctness tests still run. This is a
*test-harness* concession, not a kernel limit — production builds typically set
`config::DISABLE_ALL_TESTS`. Set `LOW_MEM_TEST_SKIP_MB = 0` to always run them.

## Size-optimized build profile

For the absolute smallest binary, use the `size` Cargo profile with the `no-tests` feature:

```bash
scripts/build_size.sh          # uses cargo +nightly, build-std, no-tests
cargo run --profile size --features no-tests -Z build-std=core,alloc  # to run in QEMU
```

**Size reduction journey (June 2026):**

| Change | Binary size |
|---|---|
| `cargo build --release` baseline | 3.6 MB |
| `[profile.size]` + `no-tests` feature (gate all `*_tests` modules) | 1.1 MB |
| `-Z build-std=core,alloc`, `panic = "immediate-abort"`, remove smoltcp `log` | 1.0 MB |
| `no-tests` activates `akuma-net/small-sockets` (MAX_SOCKETS 256→32) | **948 KB** |

**What each layer does:**

- **`[profile.size]`** — `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `strip = "symbols"`, `panic = "immediate-abort"`. The last flag converts every panic site into a single `udf` instruction, eliminating the panic formatting infrastructure.
- **`no-tests` feature** — gates all `*_tests` modules and their test-only exported symbols out of the binary entirely (not just skipped at runtime). Also activates `akuma-net/small-sockets`.
- **`-Z build-std=core,alloc`** (nightly) — rebuilds `core` and `alloc` with `opt-level = "z"` instead of the precompiled defaults.
- **`akuma-net/small-sockets`** — reduces `MAX_SOCKETS` from 256 to 32. Each socket slot is a 464-byte static `SocketStorage` entry; 256 of them occupied 116 KB of the binary's `.data` section regardless of how many sockets are actually open at runtime.

**Note on memory layout:** even at 948 KB, the kernel's boot-time memory reservation (`code_and_stack`) stays at ~11–16 MB because the boot stack is hardcoded at `KERNEL_BASE + 8 MB` in `boot.rs` and `linker.ld`. Moving the stack closer to the kernel binary would free several MB for user processes but requires changes to `boot.rs`, `linker.ld`, and `main.rs` — tracked as future work.

## Config knobs (all in `src/config.rs`)

| Const | Default | Effect |
|---|---|---|
| `KERNEL_HEAP_SIZE_MB` | 0 (auto) | Pin the kernel heap size (MiB). |
| `THREAD_LIMIT_OVERRIDE` | 0 (auto) | Pin the thread-slot count (≤ `MAX_THREADS`). |
| `LOW_MEM_TEST_SKIP_MB` | 32 | Skip heavy boot self-tests below this RAM. |
| `DISABLE_ALL_TESTS` | false | Skip the entire boot self-test suite. |

## How tcc reached 16 MB — the changes (landed June 2026)

**Status: items 1–5 landed and verified; tcc floor 48 MB → 16 MB.** This section
records what was done and why; the per-item **Done** notes give the actual file and
fix. For perspective: a 1998-era Linux box compiled C in 32 MB with room to spare —
there was no fundamental reason a 948 KB kernel + tcc couldn't do the same. What had
been eating the RAM was a handful of fixed, over-provisioned reservations sized for
"≥ 256 MB machines," not the working set of a compile. Each item below reclaimed one.
(Item 6, the `tcc -run` bug, is unrelated to memory and is still open.)

The items are ordered by **payoff ÷ effort**. The measured sweep pinned what each
fixed: **items 1–3 + 5 (user-page pressure) unlocked the 24–32 MB tiers**, where tcc
had been dying on `anon alloc failed` / ELF-load OOM after the thread pool + mmap
arenas exhausted the user pool; **item 4 (the flat 11 MB code+stack) unlocked 16 MB**,
where there simply wasn't enough RAM left for a usable heap. All five were applied on
the `size` profile (gated on `kernel_profile_size`); `release` is unchanged.

### 1. Confirm the 8 MB user stack is fully lazy — *highest payoff, highest uncertainty*

> **Done — it was EAGER.** `load_elf_with_stack` (`crates/akuma-exec/src/elf/mod.rs`
> ~611–618) calls `alloc_and_map()` → `alloc_page_zeroed()` for **every** page of
> `stack_size`, i.e. 2048 PMM pages committed per process spawn at the 8 MB override.
> Fix: under `#[cfg(kernel_profile_size)]` set `USER_STACK_SIZE_OVERRIDE = 0`, so
> `compute_user_stack_size()` returns its 128 KB minimum on small RAM — 2048 → 32
> pages, ~7.9 MB freed per process. *(The eager path is why this was the biggest win;
> a fully-lazy stack would have made it a no-op.)*

`config::USER_STACK_SIZE_OVERRIDE` is currently pinned to **`8 * 1024 * 1024`**
(`src/config.rs`). This **overrides** `compute_user_stack_size()` entirely — the
RAM-scaling that would hand out a 128 KB stack at 256 MB is dead code while the
override is non-zero, so **every** user process is given an 8 MB user-space stack
regardless of detected RAM. On a 16 MB box that is half the machine.

This is only survivable if the 8 MB is a *lazy* mapping (reserved VA, demand-paged,
zero-fill on first touch) so a freshly-loaded process commits only the few pages it
actually touches. **Verify this first** — trace `USER_STACK_SIZE_OVERRIDE` through
the ELF loader / `mmap` stack setup and confirm it goes through the lazy-region path
(`MMAP_EAGER_MAX_PAGES` gate, demand fault handler), not an eager
`alloc_pages_contiguous_zeroed`. If it pre-commits even partially, tcc cannot fit and
nothing else on this list matters.

- **If lazy:** leave the override, move on — the cost is address space, not RAM.
- **If eager (or partly):** set the override to `0` (→ 128 KB floor via
  `compute_user_stack_size`) for the low-mem / `size` profile, or make the stack
  reservation lazy. This is the single biggest RAM win on the list.

### 2. Raise the user-thread floor in `compute_thread_limit`

The 1/4-of-user-pages budget gives the small tiers almost no **user** thread slots —
only **2 at 16–24 MB**. A working session needs: the shell, the in-kernel SSH
session thread, and the `tcc` process — so 2 user slots is structurally too few
before the compile even starts.

Bump the floor from `reserved + 2` to **`reserved + 6`** (`compute_thread_limit` in
`src/main.rs`) so shell + SSH + tcc can coexist. Cost is `4 × USER_THREAD_STACK_SIZE`
= 512 KB of extra PMM pool at 128 KB/slot — cheap once item 3 shrinks the per-slot
size. Re-check `verify_stack_memory()` still passes against PMM free pages at the new
floor.

### 3. Shrink `USER_THREAD_STACK_SIZE` on the small-RAM / `size` profile

`USER_THREAD_STACK_SIZE` is **128 KB** (`src/config.rs`) — this is the *kernel-side*
syscall stack per user slot, not the process's own user stack. tcc's syscall depth is
shallow (open/read/write/mmap/brk); 128 KB is generous. Halving it to **64 KB** under
`kernel_profile_size` doubles how many slots fit the same pool, paying for item 2 and
then some. The stack-pool formula `1.75 MB + (N−8) × stack` is what to recompute; the
reserved system threads (256 KB) stay as-is since they run the deep async SSH chains.
Validate with the boot stack-canary check enabled (`ENABLE_STACK_CANARIES`) so a too-
small stack trips a canary rather than corrupting silently.

### 4. Close the 8 MB boot-stack gap — *structural, unlocks below 16 MB*

Even at a 948 KB binary, `code_and_stack` reserves **~11–16 MB** because
`BOOT_STACK_TOP` is hardcoded at `KERNEL_BASE + 8 MB` in `boot.rs` and `linker.ld`
(the `size` profile already moved `IMAGE_SIZE` 3 MB → 1 MB, but the 8 MB stack offset
above the base is untouched). Moving the boot stack adjacent to the kernel binary
frees most of that gap for user pages. Requires coordinated changes to **`boot.rs`**
(the `BOOT_STACK_TOP`/`STACK_BOTTOM` derivation), **`linker.ld`** (the
`PROVIDE(STACK_BOTTOM = …)` fallback + `ASSERT(. < STACK_BOTTOM)`), and **`main.rs`**
(the `code_and_stack` split). This is the change that takes the boot floor below
16 MB; it's also the riskiest (get the stack address wrong and boot silently corrupts).

### 5. Lower the heap floor on small RAM — *the 24 MB quick win*

`compute_heap_size` clamps to an **8 MB floor** (`clamp(ram/8, 8 MB, …)`). The sweep
shows the cost: at 24 MB RAM the kernel hands the heap 8 MB (floor) while user pages get
only **5 MB** — and tcc's ELF load then fails with `Out of memory for user page`. The
kernel only needs ~2.2 MB of heap at boot, so the 8 MB floor is over-provisioned for
this tier. Drop the floor to **~4 MB** under `kernel_profile_size` (or scale it as
`max(ram/8, 4 MB)` below ~32 MB) and the freed 4 MB goes straight to the user pool — on
the 24 MB box that nearly doubles user pages (5 → 9 MB), which is the difference between
tcc's ELF load failing and fitting. Cheapest change on the list; re-pin the
`compute_heap_size` unit test in `src/tests.rs`.

### 6. (separate bug) Fix `tcc -run` — `runmain.o not found`

Not a memory issue, but it surfaced in the sweep and blocks the most ergonomic test
path: `tcc -run /akuma-playground/hello.c` fails at **every** RAM size with
`tcc: error: file 'runmain.o' not found`. tcc's `-run` mode needs its runtime objects
(`runmain.o`/`libtcc1.a`) installed where tcc looks (`-B`/lib path). Fixing it would let
the test loop use `tcc -run` directly instead of compile-then-exec. Track separately
from the low-mem work.

### Sequencing & verification

5 and 1 → 2 → 3 are independent enough to land together and re-test as a unit (all are
config/heuristic changes); 4 is a separate, riskier change; 6 is unrelated. After 1–3+5,
re-run the `size`-profile sweep (`tcc /akuma-playground/hello.c -o /tmp/hello && /tmp/hello`
at 16/24/32 MB) and update the measured table above — **goal: the "runs tcc" column reads
`yes` at 32 MB, then 24 MB**; 16 MB needs item 4. Pin the heuristics with the
`compute_heap_size` / `compute_thread_limit` unit tests in `src/tests.rs` so the new
floors don't regress.

## Profile-aware image layout

`profile.size` produces a ~900 KB binary versus ~3 MB for `--release`, so
reserving the same 3 MB image window wastes address space and makes the linker
size assertion too loose. Two constants are computed per profile at build time.

**`IMAGE_SIZE`** — defined in `src/boot.rs`, used for both the ARM64 Image header's
`image_size` field and as the base for deriving the stack addresses:

| Profile | `IMAGE_SIZE` | `STACK_BOTTOM` | `BOOT_STACK_TOP` |
|---------|-------------|----------------|-----------------|
| `size` | `0x100000` (1 MB) | `0x40300000` | `0x40400000` |
| `release` | `0x300000` (3 MB) | `0x40500000` | `0x40600000` |

**Linker assertion** — `linker.ld` uses `PROVIDE(STACK_BOTTOM = 0x40500000)` as a
fallback; `build.rs` overrides it with `--defsym=STACK_BOTTOM=<addr>` per profile.
The `ASSERT(. < STACK_BOTTOM)` then catches an oversized binary at link time before
it would silently corrupt the boot stack.

**Profile detection** — Cargo sets `PROFILE=release` for any profile that inherits
from `release`, making `profile.size` indistinguishable at build-script level.
`build.rs` detects it via `OPT_LEVEL=z` instead (the `opt-level = "z"` in
`[profile.size]` is unique to that profile). When true it emits
`cargo:rustc-cfg=kernel_profile_size`, which flows into all `src/*.rs` and
`crates/akuma-net/` through the normal cfg machinery.

**Automatic test exclusion** — all `*_tests` modules in `src/` are gated on
`#[cfg(any(feature = "no-tests", kernel_profile_size))]`. A `profile.size` build
therefore strips test code without requiring `--features no-tests` explicitly.
`scripts/build_size.sh` passes `--features no-tests` anyway to also activate the
`akuma-net/small-sockets` path.

**`akuma-net` socket table** — `crates/akuma-net/build.rs` runs the same `OPT_LEVEL=z`
detection and emits its own `kernel_profile_size` cfg. `smoltcp_net.rs` selects
`MAX_SOCKETS = 32` under `#[cfg(any(feature = "small-sockets", kernel_profile_size))]`,
keeping the 116 KB static socket table out of the binary without an explicit
feature flag.

## Verification

`scripts/test_memory_split.py` (tcc for ≤ 1 GB, rustc for ≥ 2 GB) and the ad-hoc
small-RAM sweeps in `logs/` (`tccv9_*.log` = the 16/24/32/48 MB boot-floor run)
exercise this. Boot self-tests `compute_heap_size` and `compute_thread_limit` (in
`src/tests.rs`) pin the heuristics. See also `docs/MEMORY_LAYOUT.md` (general
layout + the RAM > 2 GB identity-map fix).

| Profile | Binary | `image_size` | `BOOT_STACK_TOP` | SSH + hello |
|---------|--------|-------------|-----------------|-------------|
| `size` | 883 KB | 1 MB | `0x40400000` | ✓ |
| `release` | 2833 KB | 3 MB | `0x40600000` | ✓ |
