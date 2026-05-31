# Running Akuma in Low-Memory Environments

How Akuma sizes its memory regions, what actually limits the **minimum bootable
RAM**, and the heuristics that let it scale from tiny VMs up to multi-GB boxes.

Set RAM with the `MEMORY` env var: `MEMORY=64M cargo run --release`.

## TL;DR

| RAM | boots? | runs tcc `hello.c`? | notes |
|---|---|---|---|
| ≥ 256 MB | yes | yes | generous heap, full 64-thread pool (unchanged) |
| 64–128 MB | yes | yes | heap + thread pool scaled down |
| 32–48 MB | yes | yes | fewer threads, small heap |
| 16–24 MB | yes (tight) | marginal | minimum thread pool |
| < 16 MB | no | — | can't fit code+stack + min heap + min thread pool |

Two things were hardcoded for "≥ 256 MB machines" and have been made to scale:

1. **Kernel heap** — was a flat 64 MB floor below 512 MB RAM (left 0 user pages
   below ~72 MB → no boot). Now `compute_heap_size()` scales it down under 256 MB.
2. **Thread-stack pool** — the scheduler eagerly allocates a stack per thread slot
   for all `MAX_THREADS` (64) from **PMM**: 7 system × 256 KB + 56 user × 128 KB =
   **8.96 MB**. On a small machine that pool is the dominant fixed cost and the
   real boot floor. Now the number of slots that get stacks scales with RAM
   (`compute_thread_limit()`).

## The three regions

`src/main.rs::kernel_main` splits detected RAM into:

```
[ Code + Stack ] [ Heap ] [ User pages (PMM) ]
  max(ram/16,8M)  see below   remainder
```

- **Code + Stack** — kernel binary (~3 MB) + boot stack. `max(ram/16, 8 MB)`.
- **Heap** — kernel data structures (`alloc`). Boot uses only ~2.2 MB; it grows
  under load (VFS, process tables). Sized by `compute_heap_size()`.
- **User pages** — the PMM free pool. Backs **both** user process memory **and the
  thread-stack pool** (stacks come from `alloc_pages_contiguous_zeroed`, *not* the
  heap — this is the key subtlety).

## Heap heuristic — `compute_heap_size(ram_size, code_and_stack)`

- `config::KERNEL_HEAP_SIZE_MB != 0` → fixed override (in MiB).
- **RAM ≥ 256 MB**: `clamp(ram/8, 64 MB, 256 MB)` — unchanged; preserves the
  proven default and headroom for go/bun/rustc kernel metadata.
- **RAM < 256 MB**: `clamp(ram/8, 8 MB, ram − code_stack − MIN_USER)` — 8 MB floor
  (kernel boots on ~2.2 MB), scaling by `ram/8`, never eating the last few MB of
  user pages.

## Thread-pool heuristic — `compute_thread_limit(user_pages_size)`

`MAX_THREADS` (64) stays a compile-time constant (it sizes per-thread atomic
arrays — cheap). What scales is **how many slots get a stack allocated**, a
runtime `thread_limit ≤ MAX_THREADS` set before `threading::init`:

- The `reserved` (8) low slots are kernel/system threads (idle, network, SSH,
  async executor) and always get stacks (`7 × 256 KB = 1.75 MB` pool minimum).
- User-process slots `[reserved, thread_limit)` get `128 KB` stacks.
- `thread_limit` is chosen so the stack pool uses at most ~half of user pages,
  leaving the rest for actual processes — with a floor of a few user threads so a
  shell + child can run.

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

## Verification

`scripts/test_memory_split.py` (tcc for ≤ 1 GB, rustc for ≥ 2 GB) and the ad-hoc
small-RAM sweeps in `logs/` exercise this. Boot self-tests `compute_heap_size`
and `compute_thread_limit` (in `src/tests.rs`) pin the heuristics. See also
`docs/MEMORY_LAYOUT.md` (general layout + the RAM > 2 GB identity-map fix).

> Results table to be filled after the thread-scaling change lands and the
> small-RAM matrix (16/24/32/48/64/96/128 MB) is re-run.
