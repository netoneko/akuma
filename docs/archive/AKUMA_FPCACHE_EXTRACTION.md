# `akuma-fpcache`: the file-page cache leaves `src/`

**2026-09-01.** `src/file_page_cache.rs` (500 lines) became
`crates/akuma-fpcache`. `src/file_page_cache.rs` remains as a 32-line shim.

This is the first move made explicitly in service of
**`#![forbid(unsafe_code)]` across `src/`**, rather than in service of
host-testability. It is worth recording *why it was picked first*, because on
the usual criterion — "does this crate get to forbid `unsafe`?" — it looks
pointless: the file already held zero `unsafe`, so nothing was localized.

## Why this file, when it had no `unsafe`

The census that started it:

```
unsafe { } blocks in src/ (non-test):   97
  src/exceptions.rs                     77   ← 79%
  src/main.rs                            7
  src/gic_v3.rs                          5
  src/gic.rs                             4
  src/smp_shared.rs                      4
```

`src/` cannot forbid `unsafe` without `exceptions.rs` leaving, and
`exceptions.rs` cannot leave while it names eight `crate::` clusters that live
in `src/`. `file_page_cache` was one of those eight. Removing it is not a
safety win in itself — it is one fewer edge on the thing that *is*.

It was picked first because it turned out to need **no dependency inversion at
all**, which the other clusters do. Its `crate::` references only looked like
`src/` dependencies:

| Reference | Count | What it actually was |
|---|---|---|
| `crate::irq::with_irqs_disabled` | 6 | `pub use akuma_primitives::irq::with_irqs_disabled` (`src/irq.rs:17`) |
| `crate::pmm::{cow_ref_inc,cow_ref_get}` | 4 | `pub use akuma_pmm::{…}` (`src/pmm.rs:30`) |
| `crate::pmm::free_count` | 1 | `akuma_pmm::free_count()` one-liner |
| `crate::pmm::free_page_at` | 3 | Wrapper supplying `current_tid()` — reproduced as a 3-line local `free_frame` |
| `crate::tprint!` | 1 | Replaced with `safe_print!`, re-exported from `akuma-primitives` |
| `crate::config::*` | 14 (4 consts) | The only real one — see below |

**Zero hooks.** Contrast with `crate::syscall::handle_syscall`, which
`exceptions.rs` needs and which is a genuine `src/`-resident service requiring a
registered fn pointer.

## The one real cost: the gate stopped being const-folded

`SHARED_FILE_PAGES_ENABLED` and the three `FPCACHE_*` tunables are
`src/config.rs` `const`s, and the crate sits below the module that owns them.
They now arrive at `init` as an `FpcacheConfig` and live in three statics, so the
seven `if !enabled()` guards are relaxed atomic loads where they used to be a
compile-time constant that deleted the branch.

`src/config.rs` remains the single source of truth for the values — the shim
reads them and hands them over, so nothing is duplicated.

Measured on `extreme-size`, A/B against the parent commit in a worktree:

| | baseline (HEAD) | with `akuma-fpcache` | Δ |
|---|---|---|---|
| file | 724,328 B | 724,328 B | **0** |
| `.text` | 600,628 | 600,932 | **+304** |
| `.data` | 51,800 | 51,800 | 0 |
| `.bss` | 415,344 | 415,360 | **+16** |

+16 `.bss` is the three new statics. +304 `.text` is the seven de-folded
branches plus the config plumbing. The image is byte-identical because of
section alignment.

An alternative was to route the flag through
`akuma_exec::runtime::config().shared_file_pages_enabled`, which already exists
and which `is_shareable_mapping` already reads. It was not taken: that call
returns a ~45-field struct **by value**, which is why `exceptions.rs` hoists it
out of per-page loops. A relaxed atomic load is cheaper than the thing the
existing caller is already documented as working around.

## What did not move, and why

`invalidate_inode` is reached from `src/vfs/mod.rs` and from ext2's
inode-freed hook. It stays `fn(u32)` and id-blind — a filesystem that knows its
own cache identity is one that can name another's. The extraction preserved
that signature exactly.

`is_shareable_mapping` still delegates to
`akuma_exec::mmu::user_flags::is_shareable_mapping` and still reads the gate at
the boundary rather than inside the predicate — the arrangement
`AKUMA_EXEC_SPLIT_AGAIN.md` §7.4 arrived at.

## Fallout: one unused import

`cow_ref_inc` lost its last production caller in this move, leaving only the CoW
refcount self-tests. `extreme-size` builds `no-tests` with `-D unused-imports`,
so `src/pmm.rs`'s re-export had to join `cow_ref_count` under
`#[allow(unused_imports)]`. Default `--release` never sees this, because
`kernel_tests` is on unless `no-tests` is set — **a `cargo check --release` that
passes proves nothing about this class of breakage.**

## Verification

- `cargo check`/`clippy` clean on host, `--release`, and `extreme-size`.
- Full host test suite green (no regressions).
- `kernel_tests` compile: `src/process_tests.rs` has 12 call sites into this
  cache and builds under default `--release`.
- **Live boot** (private `INSTANCE=1`, `MEMORY=2048`), which is the check that
  matters — a mis-wired gate is a silent no-op, not a failure:

```
[fpcache] shared file-page cache enabled, base cap=65536 pages (+20% elastic)
[FPCACHE] entries=263/78643 hits=3949 misses=268 evict=0 evict_mapped=0 inval=4
```

  `65536 = 2048 MB / 8 / 4096` proves `base_ram_divisor` travelled;
  `cap 78643 = 65536 × 1.2` proves `inflate_pct` **and**
  `inflate_headroom_mult` travelled *and* that `akuma_kacho::hysteresis` granted
  the inflation through the new atomic loads; `hits=3949` proves it serves;
  `inval=4` proves the VFS path reaches it through the shim.

## Background

- [`AKUMA_EXEC_SPLIT_AGAIN.md`](AKUMA_EXEC_SPLIT_AGAIN.md) §7.4 — why the gate
  is read at the boundary and not inside the predicate.
- [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) — the `src/syscall/`
  precedent this is following: move the operation to the crate that owns what it
  pokes, do not `#[allow]` locally.
- [`EXT2_WRITEBACK_DESIGN.md`](EXT2_WRITEBACK_DESIGN.md) F-1 — the mount-id half
  of the cache key, preserved verbatim.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the 23-of-36
  tally this run regenerated.
