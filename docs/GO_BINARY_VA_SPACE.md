# Go Binary VA Space Fix (forktest_parent OOM)

## Date

2026-03-29

---

## Problem

The `forktest_parent` Go binary (statically linked, no `PT_INTERP`) crashed at startup:

```
runtime: out of memory: cannot allocate 4194304-byte block (0 in use)
```

The Go runtime panicked during heap initialization (`internal/cpu.doinit`), before
any user code ran.  The kernel log showed:

```
[mmap] REJECT: pid=49 size=0x4000000 next=0x3cb70000 limit=0x3f700000
[mmap] REJECT: pid=49 size=0x8000000 next=0x3cb70000 limit=0x3f700000
```

Only ~43 MB of VA space remained when Go tried to allocate 64 MB heap arenas.

---

## Root Cause

### 1. compute_stack_top threshold too high

`crates/akuma-exec/src/elf/mod.rs` — `compute_stack_top(brk, has_interp)` assigned
the 1 GB default VA space to any static binary with loaded segments ending below 4 MB:

```rust
if !has_interp && brk < 0x400_0000 {   // 4 MB — too high
    return DEFAULT;  // 1 GB
}
```

`forktest_parent` is a Go statically-linked binary.  The Go runtime is embedded in the
binary but the total loaded segments ended at ~2 MB (`brk < 4 MB`), triggering the 1 GB
path.  The mmap limit was therefore only `~0x3F700000` (~1 GB).

### 2. Go arenaHint probing permanently consumes VA

During heap initialisation the Go runtime probes candidate arena base addresses
(`arenaHints`) by calling:

```
mmap(hint=4GB+k*64MB, size=64MB, PROT_NONE, MAP_ANON|MAP_PRIVATE, -1, 0)
```

On Linux, when `hint` is free the kernel returns exactly `hint`.  On Akuma, hints
are ignored — the kernel returns the next available VA from `next_mmap` instead.
Because the returned address ≠ `hint`, Go calls `munmap` to discard it and tries
the next hint.

On Akuma, `PROT_NONE` allocations are lazy (no physical pages).  By design, lazy
`munmap` does **not** recycle the VA back into `free_regions` — doing so would cause
an infinite `mmap→reject→munmap→same-addr` loop (observed with Go's heap prober
returning the same address 60+ times in a row).

Each failed probe therefore **permanently consumes 64 MB** of the bump-allocator VA.

### 3. Exhaustion arithmetic

```
VA budget:  mmap_limit (≈ 1 GB) - next_mmap_initial ≈ 1 GB
Per probe:  64 MB (one heapArenaBytes)
Probes fit: 1 GB / 64 MB ≈ 15
Go tries:   up to 128 arenaHints
```

After ~15 probes `alloc_mmap` returns `None`, the kernel returns `MAP_FAILED`, and
Go panics with "out of memory: cannot allocate 4194304-byte block (0 in use)".

---

## Fix

The threshold was lowered from 4 MB to 512 KB in `compute_stack_top`:

```rust
// Before
if !has_interp && brk < 0x400_0000 {   // 4 MB
    return DEFAULT;
}

// After
const SMALL_STATIC_THRESHOLD: usize = 0x8_0000; // 512 KB
if !has_interp && brk < SMALL_STATIC_THRESHOLD {
    return DEFAULT;
}
```

Binaries that exceed 512 KB now receive the large VA layout (128 GB mmap space,
256 GB stack top), matching dynamically-linked binaries.

### Threshold rationale

| Binary type | Typical brk | VA space assigned |
|-------------|-------------|------------------|
| musl-libc static C program | < 200 KB | 1 GB (DEFAULT) |
| TCC-compiled C program | < 200 KB | 1 GB (DEFAULT) |
| Minimal Go program (embedded runtime) | > 1 MB | Large (128 GB mmap) |
| `forktest_parent` | ~2 MB | Large (fixed) |
| Any `PT_INTERP` binary | any | Large (unchanged) |

512 KB sits safely in the gap between the two populations.  No known static C
binary (musl, uclibc, TCC) approaches 512 KB.  No Go binary can be built below
~1 MB because the Go runtime itself is ~1 MB of text + data.

---

## Files Changed

| File | Change |
|------|--------|
| `crates/akuma-exec/src/elf/mod.rs` | `compute_stack_top`: threshold `0x400_0000` → `0x8_0000`; new constant `SMALL_STATIC_THRESHOLD`; updated doc comment |
| `src/tests.rs` | Four new regression tests; registered in memory test runner |
| `docs/GO_BINARY_VA_SPACE.md` | This file |

---

## Tests Added

| Test | What it verifies |
|------|-----------------|
| `test_compute_stack_top_small_static` | `brk < 512 KB` → returns DEFAULT (1 GB) |
| `test_compute_stack_top_go_sized_static` | `brk ≥ 512 KB`, no interp → `stack_top > DEFAULT` (large VA) |
| `test_compute_stack_top_boundary_512k` | Exact boundary: 511 KB → DEFAULT; 512 KB → large VA |
| `test_go_binary_va_exhaustion_scenario` | 1 GB budget fits < 128 × 64 MB probes; large VA budget fits all 128 |

---

## Related

- `docs/EPOLL_EL1_CRASH_FIX.md` — related process crash fixes
- `src/syscall/mem.rs` — `sys_mmap` REJECT logging and lazy-PROT_NONE non-recycling
- `crates/akuma-exec/src/process/types.rs` — `ProcessMemory::alloc_mmap` bump allocator
