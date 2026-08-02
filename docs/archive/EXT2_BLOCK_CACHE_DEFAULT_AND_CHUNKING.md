# The ext2 block cache was never on — 2026-08-02

`rustc` startup on Akuma was ~58× slower than Docker. The cause was not the ELF
loader, not `mmap`, and not the BKL: the **large ext2 block cache was not compiled
into any shipping build**, so every demand-page fault re-read its own indirect
blocks off virtio-blk. Enabling it made `hello_std` **2.7×** faster and dropped the
RAM floor for the whole `rustc` workload from >2 GB to **1 GB**.

Enabling it also exposed a second defect in the cache itself — a geometric
single-allocation backing store that grew to 512 MB and destabilised the system.
Both are recorded here.

Companion to [`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md); this
closes that doc's §6 follow-up 3 ("startup cost is a bigger lever than the BKL").

## 1. What was wrong: the cache was opt-in and nothing opted in

`fs-cache` appeared exactly once in the root `Cargo.toml` — as its own feature
definition. It was **not** in `default`, not in `devbox-smoltcp`
(`= ["userspace-sshd", "smp-shared"]`), and not reachable transitively. Every
shipping build therefore compiled `akuma-ext2` without `cfg(ext2_fs_cache)` and fell
back to `BlockRingCache`:

```
crates/akuma-ext2/src/ext2.rs:54   const BLOCK_CACHE_ENTRIES: usize = 64;
```

**64 slots × 4 KB = 256 KB, FIFO eviction, linear-scan lookup.** The feature's own
doc comment described it as "opt-in (not in any default set)", so this was
deliberate — but nothing ever turned it on, including the `release` and devbox
builds it was written for. The `min(25% RAM, 512 MB)` sizing in `src/fs.rs` had
never run in anger.

### Why 256 KB is pathological here

A single file-backed demand fault reads **1 MB** (`READAHEAD_PAGES = 256`,
`src/exceptions.rs:3557` for the instruction-abort arm, `:3015` for data aborts).
That is 4× the entire cache. Consequences, all at once:

1. **Indirect blocks thrash.** `libLLVM.so.22.1` is 176 MB, so with 4 KB blocks
   essentially every logical block lands in the **double-indirect** range
   (`ext2.rs:1149`). Each `get_block_num` therefore needs two extra `read_block`
   calls. Those should stay permanently cache-hot; instead one fault pushes 256 data
   blocks through 64 slots and evicts them ~4× while filling.
2. **No readahead reuse.** A fault's own 256 blocks are gone before the next fault
   starts.
3. **No cross-process reuse.** Each of the many `rustc`/`cc`/`ld` spawns re-read
   everything the previous one had read.

The fault path compounds this by calling `read_at_by_inode` **once per 4 KB page**
(`exceptions.rs:3604-3643`), so ext2's own batching — it resolves the whole range's
block map upfront (`ext2.rs:1721`) and coalesces contiguous physical runs into one
disk read (`ext2.rs:1767`) — never sees more than one page and cannot engage.

## 2. Measured cost, before

`devbox.img`, `release-smp-shared --features devbox-smoltcp,no-tests`,
`MEMORY=4096 SMP=2`. Fault counts and per-fault cost from the unconditional
`[IA-DP]` trace.

`rustc --version` — **zero compilation**, pure startup — took 3.40 s, of which
1.43 s was 118 instruction-abort faults at **12.1 ms each**. A `rustc -O` run took
325 faults at 8.5 ms each.

For orientation: the kernel's own work is negligible. `execve` → ld-musl's first
`mmap` is **≤10 ms**. `/usr/bin/rustc` is 134 KB and its PT_INTERP is
`/lib/ld-musl-aarch64.so.1` (723 KB); `librustc_driver` (63 MB) and `libLLVM`
(176 MB) are **DT_NEEDED**, which the kernel does not parse at all — userspace
ld-musl maps them, and the log confirms they already took the demand-paged path:

```
[T100.33] [mmap] pid=23 fd=3 file=/usr/lib/librustc_driver-...so len=0x3c8a000 (lazy-file, 7 regions)
[T100.36] [mmap] pid=23 fd=3 file=/usr/lib/libLLVM.so.22.1    len=0xa89d000 (lazy-file, 9 regions)
```

So `MMAP_FILE_BACKED_LAZY` was already doing its job. The remaining cost was
entirely below it, in ext2.

## 3. Fix part 1 — put `fs-cache` in `default`

One line in `Cargo.toml`. `size`/`extreme` pass `--no-default-features` and do not
re-add it, so they are unaffected; `akuma-ext2`'s `build.rs` additionally refuses to
combine the cfg with `extreme`.

Per-fault cost, same 118-fault workload, one process per run:

| run | ms/fault | total fault time |
|---|---|---|
| baseline (256 KB ring) | 12.1 | 1.43 s |
| fs-cache, cold | 6.6 | 0.78 s |
| 2nd run | 1.9 | 0.22 s |
| 3rd / 4th run | **0.7** | **0.08 s** |

Cold-vs-cold is 1.8×; warm is **17×** per fault. The decay across runs is a
*different process each time* hitting a warm cache — that is the cross-process reuse
the 256 KB ring could never provide.

Fault **count** is unchanged (118), as expected: only the cache changed, not the
readahead policy.

## 4. Fix part 2 — the cache's backing store was a 512 MB geometric `Vec`

Enabling the cache surfaced a latent defect. `ClockBlockCache::alloc_slot` grew its
backing one block at a time with `Vec::resize` over a single contiguous `Vec<u8>`.
`Vec` growth doubles, so at the 512 MB cap the final step was
`realloc(256 MB -> 512 MB)` with both buffers live:

```
[HEAP-R]    554MB used (realloc 268435456->536870912)
[HEAP-GROW] total=1152MB used=298MB this_req=536870912 bytes claimed=131074 pages
```

The kernel heap grew to **1152 MB** and claimed 131 074 pages in one request. PMM
went 908 518 → 678 073 free (~900 MB, never returned). The kernel survived — no
panic, no OOM, 2.6 GB still free — but **sshd began accepting connections and then
resetting at key exchange**, and the benchmark harness died after four cells where
it had completed every cell the day before.

The causal chain from the heap event to the sshd failure was **not** proven; only
that the two coincide and that the harness was clean beforehand.

Two changes:

- **`crates/akuma-ext2/src/ext2.rs`** — backing is now `Vec<Vec<u8>>` in fixed
  `CACHE_CHUNK_BYTES` (1 MB) chunks. Slot `i` lives in `chunks[i / chunk_blocks]` at
  `(i % chunk_blocks) * block_size`. A grow pushes one chunk; existing slots are
  never copied and never move. The largest single allocation is now bounded at 1 MB
  regardless of the cap.
- **`src/fs.rs`** — cap `min(25% RAM, 512 MB)` → `min(12.5% RAM, 128 MB)`. The
  measured working set is far smaller than 512 MB: `rustc --version` touches ~71
  distinct 1 MB windows.

After: largest realloc is `4096->8192` bytes, no `[HEAP-GROW]`, peak footprint
1447 MB → **959 MB**, sshd responsive throughout a full harness run.

## 5. Results

Full harness (`scripts/bkl_rustc_bench/pbench.py`), SMP=2, fresh boot, artifacts
verified. Docker column from
[`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) §3.

| cell | baseline 08-01 | after | speedup | docker |
|---|---|---|---|---|
| `nostd` c=1 | 5.83 | 3.82 | 1.5× | 0.22 |
| `nostd` c=4 | 14.75 | 4.33 | 3.4× | 0.17 |
| `std` c=1 | 13.72 | **5.15** | **2.7×** | 0.23 |
| `std` c=4 | 44.30 | 9.35 (n=1) | 4.7× | 0.22 |
| `big` c=1 | 30.12 | 21.79 | 1.4× | 9.69 |
| `big` c=4 | 95.25 | FAILED 2/2 | — | 13.14 |

The vs-Docker gap on `std` c=1 closes from ~60× to **22×**; on `big` c=1 from 3.1×
to 2.2×.

### 5.1 RAM floor: >2 GB → 1 GB

SMP=2, conc=1, artifacts verified:

| RAM | `hello_std` | `big` |
|---|---|---|
| 4 GB | 5.15 s | 21.79 s |
| **1 GB** | **4.87 s** | **21.32 s** |
| 512 MB | 5.00 s | **OOM** |

**1 GB runs `big` at the same speed as 4 GB** — the cache removed the memory
pressure that made large RAM necessary, and the smaller cap means it no longer
creates its own. At 512 MB `big` genuinely runs out:

```
[OOM] allocation of 1116408 bytes failed (heap 78MB / 79MB used) — killing process
pmm=476free/131072tot
```

Note the ratio there: at 512 MB the cap is `RAM/8 = 64 MB` against an auto-sized
kernel heap of only 78 MB, so the cache would claim ~80% of the heap. **The low-RAM
end of the formula needs its own floor** — that is probably what tips `big` over at
512 MB, and it is unaddressed.

## 6. Open

- **`big conc=4` at SMP=2 failed 2/2** where the baseline passed 2/2. This is the
  [§5.1](BKL_RUSTC_SCALING_BASELINE.md) signature (artifact absent, rustc silent,
  sshd responsive, 3 of 4 concurrent artifacts correct), which that doc measured as
  intermittent at ~1-in-6 and pre-existing. **Two samples on each side cannot
  establish a rate change** — settling it needs ~8-10 reps per side, including a
  pre-change build.
- **No boot-suite self-test for the chunked backing.** Host tests cover
  `ClockBlockCache` directly (53 in `akuma-ext2`), but per convention a kernel change
  wants coverage in `src/process_tests.rs` — filling past a chunk boundary and
  asserting slot contents survive the grow.
- **Low-RAM cap floor**, per §5.1 above.
- **Fault-path batching is still not done.** `exceptions.rs` still calls
  `read_at_by_inode` per 4 KB page. With the cache warm those calls are cheap, so
  this dropped from "top lever" to "cleanup" — but ext2's run-coalescing remains
  unreachable from the fault path.
- **Readahead is forward-only** from the faulting page. Simulated against the real
  traces, a 1 MB-*aligned* window would eliminate 45–65% of faults. Faults now cost
  0.08 s total, so there is little left to win.
- **~2.2 s of `rustc --version` is still unexplained** and is not demand paging:
  ld-musl's userspace relocation work over the two big dylibs, ~340 ms of
  mmap/mprotect syscalls, and data-abort faults, which `[IA-DP]` does not count.
  Instrumenting the DA path is the next measurement.

## Background

- [`BKL_RUSTC_SCALING_BASELINE.md`](BKL_RUSTC_SCALING_BASELINE.md) — the baseline
  this improves on; §6 follow-up 3 predicted this lever, §5.1 documents the `big`
  failure.
- [`AKUMA_SELF_HOSTING.md`](AKUMA_SELF_HOSTING.md) §7a — the earlier lazy-vs-eager
  `mmap` A/B (1.8× full compile, 6.9× startup) that `MMAP_FILE_BACKED_LAZY` came
  from. That change was already in effect here; this one sits underneath it.
- `docs/reference/subsystems/config-flags.md` — feature and env knobs.
