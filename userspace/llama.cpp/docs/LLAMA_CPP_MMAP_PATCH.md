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
