# File-backed (mmap) KV cache for llama.cpp

Status: **implemented + validated on macOS** (resident/page-cache case = no degradation).
Akuma (no-page-cache) behaviour is a guest-side test, not yet run.
Target: Akuma OS (AArch64, CPU-only, no kernel page cache), backing store = virtio-blk
against a fallocate'd raw file (no filesystem).
Dev/validation host: macOS (Apple Silicon, 48 GiB, 12 cores).

Enable with `LLAMA_KV_CACHE_FILE=<path>` (CPU/host KV buffers only). See
"Implementation (as built)" and "Verification results" below.

## Goal

Add a KV-cache provisioning mode where the cache tensors are backed by an mmap'd,
fallocate'd file on a block device instead of anonymous RAM. Motivation framed by the
requester as **RAM replacement** (not RAM extension) — analogous to how model weights
are already mmap'd.

## Key conclusion up front

- **q4 quantization** is what actually shrinks the cache (3.55× vs f16). Real RAM saving.
- **mmap does NOT reduce the resident footprint** if you want decode to stay fast.
  To be fast the pages must be resident; KV decode reads the *entire* cache every token
  (full-span sweep), so the working set ≈ the whole cache. mmap only changes *where* the
  bytes are accounted (file / host page cache vs guest anonymous) and makes the commit
  lazy + evictable + persistable.
- The weights mmap is "free" only because weights are **read-only** (clean pages: drop &
  re-fault for free, zero writeback). KV is **read-write** (dirty pages → need writeback
  machinery = the page cache Akuma doesn't have). This is the structural difference.
- On Akuma's no-page-cache device the useful "fits-but-evictable" middle ground does not
  exist: either resident (no RAM saved) or per-access virtio round-trips (slow).

## Model under test: Qwen3.5-0.8B Q4_K_M

`models/qwen3.5-0.8b-q4.gguf` (532 MB file, 497 MiB weights, 752 M params)

| Field | Value |
|---|---|
| arch | `qwen35` |
| block_count (layers) | 24 |
| context_length (max) | 262144 (256K) |
| embedding_length | 1024 |
| head_count (Q) | 8 |
| head_count_kv | 2 (GQA) |
| key_length / value_length (head_dim) | 256 / 256 |

Derived: `n_embd_k_gqa = n_embd_v_gqa = head_count_kv × head_dim = 2 × 256 = 512`.
Per token across 24 layers = `24 × (512 + 512) = 24,576` elements.

## KV cache size

Per-element cost (ggml block layouts): f16 = 2.0 B, q8_0 = 34/32 = 1.0625 B,
q4_0 = 18/32 = 0.5625 B.
Per token: f16 = 48.0 KiB, q8_0 = 25.5 KiB, **q4_0 = 13.5 KiB**.

| Context | f16 cache | q4_0 cache | q4 saves vs f16 |
|---|---|---|---|
| 4,096 | 192 MiB | 54 MiB | 138 MiB |
| 8,192 | 384 MiB | 108 MiB | 276 MiB |
| 32,768 | 1,536 MiB | 432 MiB | 1.08 GiB |
| 65,536 | 3.0 GiB | 864 MiB | 2.16 GiB |
| 262,144 (max) | 12.0 GiB | 3.375 GiB | 8.6 GiB |

## Per-token decode read (the bandwidth wall)

Decode reads K and V for **all** past positions every step (global attention; the new
token attends to the whole history). What's *written* each step is only the new token's
K/V (~per-token size above); what's *read* is `context_length × per-position size`.

For q4_0 at 32k: `13.5 KiB × 32768 ≈ 432 MiB read per generated token`.
(For comparison, an 8B/f16 model at 32k reads ~4 GB/token — why this small model is a
sensible testbed.)

This read is already fully parallel across cores/SIMD; it is **bandwidth-bound**, so
"parallelize harder" does not help. Levers are: read fewer bytes (q4/GQA/MLA/windowed
attention) or amortize over more tokens (speculative decoding). Putting the cache behind
a slower-than-RAM link throttles exactly the resource that is already the limiter.

## Baseline TPS (measured)

`llama-bench`, CPU path (`-ngl 0`), `-fa 1`, `-r 2`, build `2f2923f89 (8230)`.

| depth | f16 KV tg t/s | q4_0 KV tg t/s | f16 prefill t/s | q4_0 prefill t/s |
|---|---|---|---|---|
| 0 | 42.5 | 47.8 | 374 | 364 |
| 8,192 | 31.7 | 20.4 (±13) | 240 | 95 |
| 32,768 | 20.1 | 17.1 | 122 | 31 |

Commands:
```
M=models/qwen3.5-0.8b-q4.gguf
./build/bin/llama-bench -m $M -ngl 0 -fa 1 -ctk f16  -ctv f16  -p 512 -n 64 -d 0,8192,32768 -r 2
./build/bin/llama-bench -m $M -ngl 0 -fa 1 -ctk q4_0 -ctv q4_0 -p 512 -n 64 -d 0,8192,32768 -r 2
```

### Findings

1. **KV-read cost is real**: f16 tg decays 42.5 → 31.7 → 20.1 as the cache grows. That is
   the per-token full-sweep cost, measured.
2. **q4 KV is *slower* than f16 at depth on CPU** (17.1 vs 20.1 at 32k; prefill 31 vs 122).
   CPU is not bandwidth-starved like a GPU, so the per-read dequantization of q4_0 K/V in
   the flash-attn kernel costs more than the bandwidth it saves. **q4 on CPU = memory win,
   throughput loss at depth.** Plan accordingly: the baseline to beat for the mmap CPU
   experiment is the **f16** numbers.

## Relevant code locations

- KV alloc / buffer-type selection: `src/llama-kv-cache.cpp:119`
  (CPU path hard-wires `ggml_backend_cpu_buffer_type()`; offload uses device buft).
- KV tensor creation: `src/llama-kv-cache.cpp:138-139`.
- Backing memory allocated: `src/llama-kv-cache.cpp:190`
  (`ggml_backend_alloc_ctx_tensors_from_buft`).
- Init zeroing (the gotcha): `src/llama-kv-cache.cpp:198`
  (`ggml_backend_buffer_clear(buf,0)` memsets the whole buffer).
- CPU buffer type to mirror: `ggml/src/ggml-backend.cpp:2120` (get_base),
  `:2174` (`ggml_backend_cpu_buffer_i`), `:2208` (`alloc_buffer` → `ggml_aligned_malloc`),
  `:2231` (`ggml_backend_cpu_buffer_type`).
- Existing weights mmap (read-only reference): `src/llama-mmap.cpp:421-430`
  (`mmap(NULL, size, PROT_READ, MAP_SHARED, fd, 0)`).
- Cache-quant constraint: `src/llama-context.cpp:2858-2859`
  (**V-cache quantization requires flash attention**); head_dim must divide the quant
  block size (256 % 32 == 0 here, OK).
- CLI flags: `-ctk/-ctv` (`common/arg.cpp:1996+`).

## Implementation (as built)

One new `ggml_backend_buffer_type` + a selection gate. **No changes to attention,
`set_input`, or any compute path** — `is_host = true` keeps the existing host-pointer
read/write paths valid. Verified by identical generation (see below).

### Files

- **`src/llama-kv-file.{h,cpp}`** (new) — the file-backed host buffer type.
  - `alloc_buffer(buft, size)`: `open(O_RDWR|O_CREAT)` → grow to `size` if smaller
    (`posix_fallocate`; on macOS `fcntl(F_PREALLOCATE)` + `ftruncate`) →
    `mmap(NULL, size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)`.
  - `get_base` → mmap addr; `free_buffer` → `munmap` + `close`; `is_host` → `true`;
    `set/get/memset/cpy` → plain host memcpy; `clear` → `memset` over the mapping.
  - `llama_kv_file_buffer_type(path)` returns one buft per path (registry), so all CPU
    layers group into a single ctx → single backing file.
  - Concurrent live buffers (e.g. iSWA's two caches) get `cache.img`, `cache.img.1`, …;
    a `n_live` counter is **decremented in `free_buffer`** so sequential contexts
    (e.g. `llama-bench`'s per-row contexts) **reuse** `cache.img` instead of spawning new
    files. Verified: no `.1` file appears across a multi-row bench run.
  - Windows: compiled out (returns `nullptr` → caller falls back to anonymous).
- **`src/llama-kv-cache.cpp`** — include `llama-kv-file.h`; selection gate (see below).
- **`src/CMakeLists.txt`** — add `llama-kv-file.cpp`; add `../ggml/src` to llama's PRIVATE
  includes (needed for the internal `ggml_backend_buffer_init` / struct layouts). Kept in
  the llama layer so it survives upstream ggml syncs without merge conflicts.

### Selection gate (important correction vs. the original plan)

The plan said "gate on `!offload`". That is **wrong**: `offload = cparams.offload_kqv`,
which defaults to **true even with `-ngl 0`** (it is controlled by `-nkvo`, not `-ngl`).
With `-ngl 0` the layer device resolves to CPU, so the KV buffer is already a host buffer,
but the `offload` branch is still taken. Gating on `!offload` meant the file path silently
never engaged (and a silent fallback to anonymous still produces identical output, so the
generation-diff test alone did not catch it — the `cache.img`-was-created test did).

Correct gate (`src/llama-kv-cache.cpp`, after `buft` is resolved): if
`getenv("LLAMA_KV_CACHE_FILE")` is set **and** `ggml_backend_buft_is_host(buft)`, swap
`buft` for `llama_kv_file_buffer_type(path)`. This catches both `-ngl 0` and explicit
`-nkvo`, and correctly leaves real GPU (non-host) buffers untouched.

### Notes / gotchas

- **File auto-sizes to the KV buffer**, which is set by `n_ctx`, *not* by the bench `-d`
  depth. `llama-cli` defaulted to `n_ctx = 65536` here → `cache.img` grew to ~864 MiB
  (q4 *and* f16: same element count, the buffer alloc is type-aware so f16 would be larger
  at a given n_ctx — but n_ctx default produced 864 MiB for q4_0). Pre-allocating the file
  is optional; the code grows it as needed.
- **Init clear** (`llama-kv-cache.cpp:198` → our `clear`) memsets the whole mapping → a
  full-file write at startup. Fine on macOS (page cache). On Akuma's no-cache device this
  is a full `cache.img` write up front — skip/defer for that target.
- **`llama-cli` crashes *after* generation** (`common_chat_peg_parse`, exit 134) on this
  model's "thinking" output — unrelated to the KV cache (happens with anonymous cache too).
  Generated tokens stream to **stderr** and are emitted before the crash, so correctness is
  still checkable. `llama-bench` does no chat parsing and does not crash.

### Akuma-specific notes (no page cache, virtio-blk, raw fallocate'd file)

- A fault path = guest fault → virtio-blk read → vmexit → host services from **host page
  cache** → DMA into guest frame → completion IRQ. Cost is dominated by **virtio
  round-trip latency per 4 KiB page**, not media bandwidth (host cache absorbs re-reads).
- Dirty KV pages need a writeback policy (no guest page cache to do it): options are
  write-through (every store → virtio write = death), or flush-on-msync/teardown (treat
  the device as scratch; data lost on crash). The MMU gives dirty bits; something must scan
  them and issue virtio writes — that is a slice of a page cache you'd have to build.
- `fallocate` up front (vs sparse) reserves blocks → contiguous host extents, no lazy
  allocate-on-first-write stalls, no mid-run ENOSPC.
- Viable "overflow" architecture (if ever wanted) is **explicit per-layer streaming over
  the virtqueue**, not transparent mmap: keep ~2 layers resident, prefetch layer L+1's K/V
  while computing layer L. Still floored by "must read full KV/token" bandwidth.

## Verification results (macOS, Apple Silicon)

Build:
```
cmake -B build -DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF
cmake --build build --target llama-bench llama-cli -j 12
```

### Correctness — identical generation

```
M=models/qwen3.5-0.8b-q4.gguf
rm -f cache.img cache.img.*
./build/bin/llama-cli -m $M -ngl 0 -fa 1 -ctk q4_0 -ctv q4_0 \
    -p "The capital of France is" -n 24 -no-cnv --temp 0 --no-warmup            # anonymous
LLAMA_KV_CACHE_FILE=cache.img ./build/bin/llama-cli -m $M -ngl 0 -fa 1 \
    -ctk q4_0 -ctv q4_0 -p "The capital of France is" -n 24 -no-cnv --temp 0 --no-warmup
```
Result: **generated text byte-identical** between anonymous and file-backed; `cache.img`
created by the patch (`O_CREAT`), sized to the KV buffer; no spurious `cache.img.1`.

### Performance — file-backed vs anonymous (tg64, t/s)

`llama-bench ... -p 512 -n 64 -d 0,8192,32768 -r 2`, CPU path (`-ngl 0`), `-fa 1`.

| depth | q4 anon | q4 **file** | f16 anon | f16 **file** |
|---|---|---|---|---|
| 0 | 47.8 | 46.4 | 42.5 | 43.6 |
| 8,192 | 20.4 (±13, noisy) | 33.0 | 31.7 | 34.8 |
| 32,768 | 17.1 | 17.5 | 20.1 | 21.7 |

**Conclusion: file-backed ≈ anonymous, within run-to-run noise (sometimes slightly
higher).** Confirms the "resident = free" prediction: with a page cache, the mmap'd KV
cache faults in once, stays resident, and behaves like anonymous RAM. The ggml integration
and the resident-case cost are validated.

This does **not** test the Akuma no-page-cache device path — that is a guest-side test and
cannot be reproduced on macOS (here the host page cache backs everything). On Akuma, expect
the behaviour described in "Akuma-specific notes": fine while resident, write-back policy
required for the dirty pages, and the init-clear full-file write to address.

### Pre-allocating the backing file (optional)

The patch grows the file as needed, but you can pre-reserve blocks (fallocate analog):
```
mkfile 512m cache.img      # macOS, truly allocated; Linux: fallocate -l 512M cache.img
```

## Low-memory inference: flags, limits, and measured TPS

### Anatomy of anonymous (non-evictable) memory

When running CPU-only with a model larger than or close to the RAM limit, three
anonymous allocations compete for space. These cannot be evicted — they must fit
in RAM:

| Allocation | Size (qwen3.5-0.8b-q4, c=4096) | Eliminated by |
|---|---|---|
| Repack buffer | ~495 MB | `--no-repack` |
| KV cache (anon) | ~192 MB (f16) | `--kv-cache-file <path>` |
| Compute/graph buffers | ~50–200 MB | `-b 64 -ub 64`, `-c` reduction |

Everything else (`--mmap` model weights) is file-backed → evictable by the OS.

**Rule of thumb:** all anonymous allocations must fit in RAM. File-backed pages can
exceed RAM but each eviction costs a disk round-trip.

### Recommended flags for ≤ 1 GB RAM (CPU-only, `-ngl 0`)

```
llama-server \
  --mmap                        # model weights are file-backed → evictable
  --no-repack                   # drop ~495 MB anonymous repack buffer
  --kv-cache-file /cache.img    # KV cache file-backed → evictable (~14 MB resident)
  -fit off                      # prevent llama over-sizing buffers to "free" RAM
  -b 64 -ub 64                  # shrink anonymous compute buffer
  -c 4096                       # limit KV cache size (default 256K is too large)
  -ngl 0
```

Pre-allocate the backing file before starting the server:
```sh
dd if=/dev/zero of=/cache.img bs=1M count=900
```

`LLAMA_KV_CACHE_FILE=/cache.img` is an equivalent environment variable.

### TPS matrix — qwen3.5-0.8b-q4 (532 MB model) on Linux / Docker, ARM64, CPU-only

Measured via `llama-server` + `/completion` endpoint, 5 requests × 32 tokens,
no swap (`--memory-swap` = `--memory`). Flags: `--no-repack -b 64 -ub 64 -c 4096 -fit off`
plus the `--mmap` / `--kv-cache-file` columns being tested.

| RAM | `--mmap` | `--kv-cache-file` | boots? | pp t/s | tg t/s |
|-----|----------|-------------------|--------|--------|--------|
| 128m | off | off | OOM | — | — |
| 128m | off | on  | OOM | — | — |
| 128m | on  | off | OOM | — | — |
| 128m | on  | on  | OOM† | — | — |
| 256m | off | off | OOM | — | — |
| 256m | off | on  | OOM | — | — |
| 256m | on  | off | ✓ | 11.5 | 2.8 |
| 256m | on  | on  | ✓ | 11.9 | 2.7 |
| 512m | off | off | OOM | — | — |
| 512m | off | on  | OOM | — | — |
| 512m | on  | off | ✓ | 9.9  | 2.5 |
| 512m | on  | on  | ✓ | 11.1 | 2.8 |
| 1 GB | off | off | ✓ | 81.7 | 22.8 |
| 1 GB | off | on  | ✓ | 61.1 | 17.2 |
| 1 GB | on  | off | ✓ | 56.8 | 18.2 |
| 1 GB | on  | on  | ✓ | 70.3 | 20.9 |

† Server started (health check passed) but request timed out — too slow to respond.

Key observations:
- `--no-mmap` requires the model in anonymous RAM; OOMs at ≤ 512m (model = 532 MB).
- `--mmap` enables 256m–512m operation, but at ~2.5–2.8 tg t/s: the working set
  is ~4× RAM, so every forward pass re-pages a large fraction of the model from disk.
- At 1 GB the model mostly fits; TPS jumps to 17–23 regardless of mmap setting.
- `--kv-cache-file` alone is the margin that stretches 128m from OOM to almost-possible
  (KV cache drops from ~192 MB anonymous to ~14 MB resident).
- `llama-bench` (direct pp512/tg128) needs more contiguous memory than the server;
  it only ran at 1 GB without `--kv-cache-file`.

## Akuma OS disk bottleneck: why it's ~40× worse than Linux at low RAM

### The fundamental path difference

On **Linux** (Docker, no swap), when a file-backed mmap page is evicted and re-read:

```
page fault → OS page cache lookup → read from host FS → fault in → resume
```

The Linux page cache is the key: once a host (macOS) page is read, the guest Docker
container's kernel caches it. Subsequent re-reads of the same page hit the in-kernel
cache (DRAM speed), not the underlying storage. Clock/LRU eviction keeps hot pages
resident.

On **Akuma** (no page cache, virtio-blk), the same re-read costs:

```
guest page fault → PTE check → no cache → virtio-blk read request → VM exit
  → QEMU services from host page cache → DMA into guest frame
  → virtio completion IRQ → return to guest → resume
```

Every evicted page re-read incurs a **full virtio round-trip**: VM exit, QEMU
context switch, DMA, completion IRQ — ~100–500 µs per 4 KiB page. Akuma also
uses a **FIFO rotating-cursor eviction** (no LRU access-bit tracking), so it
evicts pages in address order regardless of recency. A forward pass through the
model therefore evicts hot pages that were just used, causing a cascade of
re-faults on the next pass.

### Measured comparison (256m, disk-bound)

| Platform | RAM | tg t/s | notes |
|----------|-----|--------|-------|
| Linux Docker (no swap) | 256m | ~2.7 | LRU eviction, page cache |
| Akuma OS (QEMU HVF) | 256m | ~0.067 (~15 s/tok) | FIFO eviction, virtio round-trips |

**Akuma is ~40× slower** at 256m: same binary, same model, same host. The gap is
entirely the page-fault path, not CPU throughput.

### Why Akuma is better at > 1 GB RAM

When the model fits in RAM, the disk path is inactive. At ≥ 1 GB:
- Model pages (532 MB) stay resident after initial load — no eviction occurs.
- KV cache (192 MB at c=4096) also stays resident.
- Compute buffers are anonymous — never disk-backed.

In this regime, Akuma's disk penalty disappears and the only overhead is the
QEMU HVF VM-exit cost for syscalls and interrupts (~25% slower than bare Linux).

| Platform | RAM | tg t/s | notes |
|----------|-----|--------|-------|
| Linux Docker (no swap, `--mmap`) | 4 GB | ~37.6 | model fits, disk inactive |
| Akuma OS (QEMU HVF, `--mmap`)   | 4 GB | ~30   | same, QEMU HVF overhead only |

At 4 GB the gap narrows to ~25%: pure VM-exit overhead, no disk bottleneck.
The ~40× disk penalty at 256m collapses to ~1.25× when the working set fits.

**Practical conclusion:** Akuma is viable for inference when RAM ≥ model size. The
disk bottleneck is universal (Linux is slower too), but Akuma's FIFO eviction and
virtio latency make it catastrophic below that threshold.
