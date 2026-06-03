# Running Akuma in Low-Memory Environments

How Akuma sizes its memory regions, what actually limits the **minimum bootable
RAM**, and the heuristics that let it scale from tiny VMs up to multi-GB boxes.

Set RAM with the `MEMORY` env var: `MEMORY=64M cargo run --release`.

## TL;DR

Verified with `scripts/test_memory_split.py` + the small-RAM sweeps in `logs/`
(tcc compiling `/akuma-playground/hello.c`):

| RAM | boots to SSH? | runs tcc `hello.c`? | heap / user / threads | notes |
|---|---|---|---|---|
| ≥ 256 MB | yes | **yes** | 64 MB / large / 64 | generous heap, full pool (unchanged) |
| 48–128 MB | yes | not yet¹ | 8 MB / 32 MB / 58 (@48M) | boots fine; tcc run not completing — see Future work |
| 32 MB | yes | not yet¹ | 8 MB / 16 MB / 26 | boots (self-tests auto-skipped ≤32 MB) |
| 16–24 MB | yes | no¹ | 4–8 MB / 4–8 MB / 10 | boots; only **2 user thread slots** — see Future work |
| 12 MB | **no** | — | 1 MB / 3 MB / 10 | heap cap starves it; below the floor |

¹ **Boot ("boot at all") works down to 16 MB.** Running `tcc` on 16–128 MB does
not complete yet — at 16–24 MB there are only 2 user thread slots, and the small
heap/user split is tight. This is tracked under **Future work** below; the focus
of this change was making the kernel *boot* across the range, which it now does
from 16 MB up. tcc is verified working at ≥ 256 MB.

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
- `thread_limit` is chosen so the stack pool uses **at most ~1/4 of user pages**,
  leaving the rest for actual processes — with a floor of `reserved + 2` so a
  shell + child can still run. (An earlier 1/2 budget was too greedy: on a 32 MB
  box it allocated 58 threads / 8 MB of stacks and the first process ELF load
  then OOM'd.)

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

## Future work — more user thread slots for 16/32 MB

The kernel now **boots to SSH from 16 MB up**, but `tcc` doesn't run yet on the
16–32 MB tier. The `compute_thread_limit` 1/4-budget gives those tiers very few
**user** thread slots (only 2 at 16–24 MB), which is too few once you account for
the shell, the in-kernel SSH session, and the compiler process — and the small
user-page pool leaves little for the process image.

Next time, for the 16/32 MB profiles specifically: **raise the user-thread floor**
(e.g. `reserved + 6..8` instead of `reserved + 2`) so a shell + SSH + tcc can
coexist, and/or trim per-thread stack sizes on small RAM (a smaller
`USER_THREAD_STACK_SIZE` would let more slots fit in the same pool). Likely also
worth confirming the 8 MB user-stack reservation is fully lazy so a process image
doesn't pre-commit it. Goal: actually compile `hello.c` at 16/32 MB, not just boot.

## Verification

`scripts/test_memory_split.py` (tcc for ≤ 1 GB, rustc for ≥ 2 GB) and the ad-hoc
small-RAM sweeps in `logs/` (`tccv9_*.log` = the 16/24/32/48 MB boot-floor run)
exercise this. Boot self-tests `compute_heap_size` and `compute_thread_limit` (in
`src/tests.rs`) pin the heuristics. See also `docs/MEMORY_LAYOUT.md` (general
layout + the RAM > 2 GB identity-map fix).

> Results table to be filled after the thread-scaling change lands and the
> small-RAM matrix (16/24/32/48/64/96/128 MB) is re-run.
