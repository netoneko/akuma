# Running Akuma in Low-Memory Environments

How Akuma sizes its memory regions, what actually limits the **minimum bootable
RAM**, and the heuristics that let it scale from tiny VMs up to multi-GB boxes.

Set RAM with the `MEMORY` env var: `MEMORY=64M cargo run --release`.

## TL;DR

### 🏁 Milestone — meow + tcc agentic floor down to 4.5 MB (June 2026)

With the **optimized static tcc** (`userspace/tcc`, 603 KB → 291 KB), the
**apk-musl toolchain** (musl from `apk add musl-dev`; we ship only `libtcc1.tar`),
and meow's **file-backed tool output**, the floors dropped again — re-measured on
the `extreme` profile with `scripts/our_tcc_floor.py`:

| Path | Floor | Notes |
|---|---|---|
| `tcc -static` compiles + runs `hello.c` **and** `hello_stripped.c` (alone) | **4.5 MB** | identical floor for `printf` and bare-`write` → libc size doesn't move it |
| meow `-c "say hi"` (alone) | **4.5 MB** | |
| **meow agentically drives `tcc -static` + runs the binary** | **4.5 MB** | clean, `panic=0` (`logs/4.5mb_meow5.log`, 2026-06-05). Low-water 1988 KB; settled 2520 KB free. SSH read gaps reached ~1.6 s in that log — caused by ext2 block-cache heap fragmentation (now fixed, see *ext2 block-cache fix* below) |

The earlier 5 MB meow→tcc floor was in `logs/meow5mb.log`. The 4.5 MB record
is `logs/4.5mb_meow5.log`.

### 🏁 Milestone — floor pushed to 4 MB (June 2026)

Changing the ARM64 Image header `text_offset` from `0` to `0x100000` (1 MB) moves the
QEMU kernel load point from `RAM_BASE + 2 MB` to `RAM_BASE + 1 MB`, which shifts the DTB
placement from `0x40400000` to `0x40200000`. At 4 MB (ram_end `0x40400000`) the old DTB
address was at the boundary (QEMU check: `dtb_start < ram_end` fails), preventing boot.
The new placement `0x40200000 < 0x40400000` passes — **extreme boots at 4 MB**.

Verified (`logs/4mb_meow0.log`, 2026-06-05, kernel 801 KB):

| Workload | Result | Notes |
|---|---|---|
| boot + SSH | ✅ | 664 PMM free pages; DTB at 0x40200000 |
| `meow -c "say hi"` | ✅ | 3 sessions; connects to ollama, streams reply; low-water 1804 KB free |

meow's minimum floor is now the **kernel boot floor**: 4 MB. (tcc/libc compiles still need
~6 MB for their working set; only the light single-request path drops here.)
Full write-up: [TCC_LOW_MEMORY.md](TCC_LOW_MEMORY.md).

### 🛡️ OOM hardening — kernel survives, kills the process (June 2026)

Previously, at 4.5 MB the meow→tcc path drained the PMM to ~0 and the kernel's
*own* next allocation failed → whole-kernel `BRK` abort (`EC=0x3c` from EL1,
`4.5mb_meow2.log`). Fixed by a small **PMM emergency reserve** that user
demand-paging won't dip into:

- `pmm::USER_PAGE_RESERVE` (16 pages) + `alloc_page_zeroed_user()` — user
  demand-paging fault fills (anon + file, data + instruction abort) return `None`
  once free PMM hits the reserve, so the faulting process is **SIGSEGV'd** via the
  existing path. Page tables, kernel-heap growth, and the kill path stay on the
  reserve-exempt `alloc_page_zeroed()`.
- `allocator::handle_oom` grows the heap by just `needed` (not a 64-page chunk)
  when PMM is critically low, so the kill path can still allocate from the reserve.
- Self-test: `test_oom_user_page_reserve` (`src/process_tests.rs`).

**Validated** (`logs/oom_patch_4p5mb.log`): 4.5 MB meow→tcc now shows **0** crash
markers; the over-demanding processes SIGSEGV gracefully and SSH keeps serving.

**Still open:** memory **reclaim after process death** is incomplete — post-OOM at
4.5 MB even a trivial `busybox` SIGSEGVs because the dead processes' pages aren't
fully returned to the PMM (the "permanent high-water" reclaim issue). So 4.5 MB
stays *unusable* after an OOM, but the kernel no longer dies. That reclaim gap is
the next lever below 5 MB — **not** the toolchain.

### 🛡️ Heap-growth backoff — fragmentation no longer aborts the kernel (June 2026)

A *second* `EC=0x3c` `brk #1` abort surfaced at the 4 MB meow+tcc floor
(`4mb_meow_tcc0.log`), and it is **not** the same as the reserve crash above. Here
the PMM was **not** exhausted — the crash dump shows **108 free pages (432 KB)** —
yet the kernel's own `Box`/`Vec` allocation aborted:

```
[Exception] Sync from EL1: EC=0x3c, ISS=0x1
  ELR=0x4017ca98 … Instruction at ELR: 0xd4200020   ; brk #1
  PMM: 108/1024 pages free (432 KB / 4096 KB)
  Heap: 403349/1929216 bytes used (2649683 allocs, peak=543637)
```

**Root cause — physical fragmentation, not exhaustion.** The kernel heap lives in
the linear (`phys_to_virt`) map, so a Talc heap span must be *physically*
contiguous. `PmmOomHandler::handle_oom` grew the heap by claiming a contiguous run
of pages from the PMM. After **2.6 M tiny, churning allocations** (the meow→ollama
network-buffer path), the PMM bitmap is a checkerboard: 100+ pages free, but no
long contiguous run. `alloc_pages_contiguous_zeroed(n)` returned `None`, so
`handle_oom` returned `Err` → the global allocator returned null → Rust aborted
the **whole kernel**, even though a single free page was available to satisfy the
(small) allocation.

The `USER_PAGE_RESERVE` from the previous section does not cover this: that
reserve gates *user* demand-paging, but heap growth is a *kernel-internal*
(reserve-exempt) caller. The reserve keeps single pages available — exactly the
pages `handle_oom` then refused to use because it demanded them contiguous.

**Fix — back off the contiguous run length toward `needed`** (`src/allocator.rs`,
`handle_oom`):

- Start at the amortised `HEAP_GROW_PAGES` (64) run when memory is ample, or
  exactly `needed` under pressure (unchanged).
- On a contiguous-allocation failure, **halve `n` toward `needed`** and retry,
  instead of giving up. Any layout that fits in one page (`needed == 1` — the
  dominant case) thus grows as long as *any* single page is free; larger layouts
  get the largest run still formable.
- Only a genuine **multi-page-contiguous shortfall** (true fragmentation OOM)
  falls through to `Err`. That residual case is what the **user-process OOM
  killer** (planned) will hook into — it is the one remaining path that can still
  abort the kernel.

The decision is split into two pure, unit-testable helpers —
`heap_grow_initial_pages(needed, free)` and `heap_grow_backoff(n, needed)` — so the
boundaries are pinned without draining real RAM.

- Self-test: `test_heap_grow_backoff_plan` (`src/process_tests.rs`) — asserts the
  single-page layout backs off all the way to 1, the multi-page layout clamps at
  `needed`, and the loop always terminates.

**Why this is the right floor fix:** small-RAM workloads fragment the PMM faster
than they exhaust it. Turning "no 64-page run" into "claim the page that *is*
free" removes a whole class of spurious aborts where memory was actually
available. The genuinely-out-of-contiguous-memory case remains, and is the OOM
killer's job (see `docs/OOM_KILLER_PLAN.md`).

**Validated (2026-06-06, live ollama, `logs/oomfix/boot_{3,4,5}mb.log`).** Three
`extreme` VMs booted in parallel at 3 / 4 / 5 MB against a running ollama
(`qwen3:4b` on the host gateway). Across the whole session — repeated
`meow→ollama` streaming generations and direct `tcc` compiles — **all three
reported `panic=0` and zero crash markers** (no `Sync from EL1`, no `EC=0x3c`, no
`brk #1`):

| RAM | boot + SSH | heavy `meow→ollama` stream | `tcc -static` compile **and run** | crash markers | peak heap |
|-----|-----------|----------------------------|-----------------------------------|---------------|-----------|
| 3 MB | ✅ (serves SSH) | — (not exercised) | — (not exercised) | **0** | 142 KB |
| 4 MB | ✅ | ✅ (24 conns) | ✅ → `Hello, Akuma!` | **0** | 268 KB |
| 5 MB | ✅ | ✅ | ✅ → `Hello, Akuma!` | **0** | 212 KB |

Two notable observations with the backoff fix in place:
- A **direct `tcc -static` compile + run succeeded at 4.0 MB**
  (`/akuma-playground/hello.c` → `Hello, Akuma!`), below the previously documented
  4.5 MB direct-tcc floor. Single-run observation — treat as "floor lowered,
  confirm repeatability" rather than a hardened number.
- **3 MB boots to a serving SSH** (below the documented 4.0 MB boot floor), though
  meow / tcc were not exercised there.

Caveat — the **full *agentic* meow+tcc pipeline** (meow writes the `.c`, then calls
`tcc`) was *not* completed end-to-end at 4 MB: the model misbehaved (emitted a
broken `sh -c` and a no-op edit, then hit max tool-iterations) and a JSON-unescape
bug left literal `<` in its generated source. Those are userspace/model
issues, not kernel aborts — the kernel stayed up throughout. The agentic floor
headline above (4.5 MB) is therefore unchanged; only the *direct* tcc path was
observed lower.

### Network syscall allocations — "we have a fixed buffer, why does traffic allocate?"

A natural question from the crash logs: the `Allocs:` counter explodes during a
meow→ollama run (107 K → **2.6 M** allocations in `4mb_meow_tcc0.log`) even though
the network stack uses *fixed* buffers. Both facts are true — they are about
different buffers.

**What is fixed.** The smoltcp socket ring buffers are allocated **once per
socket** at creation (`TCP_RX_BUFFER_SIZE` / `TCP_TX_BUFFER_SIZE`,
`crates/akuma-net/src/smoltcp_net.rs`), and the VirtIO device DMA buffers are
fixed `[u8; 2048]` arrays inside `VirtioSmoltcpDevice`. The device RX/TX token
path and `poll()` are allocation-free in steady state. So the *stack* is fixed —
that part is correct.

**What is not fixed — the syscall boundary.** Every `read`/`write`/`recv`/`send`
on a socket fd allocates a **fresh transient kernel bounce buffer** to copy data
across the user↔kernel address-space boundary:

```rust
// src/syscall/fs.rs  (socket branch of sys_read)
let to_read = count.min(64 * 1024);
let mut temp = alloc::vec![0u8; to_read];      // alloc
socket::socket_recv(idx, &mut temp, nonblock); // smoltcp copies ring → temp
copy_to_user_safe(buf_ptr, temp.as_ptr(), n);  // temp → user
// temp dropped here                            // free
```

The same pattern is in `sys_write`, `sys_sendto`, and `sys_recvfrom`
(`src/syscall/net.rs`). The buffer is **allocated and freed within the single
syscall**, which is why **heap *used* stays flat (~124 KB) while the cumulative
`Allocs` counter races** — this is allocation *churn*, not a leak.

**Why so many.** An LLM streaming response arrives as many small HTTP/SSE chunks;
meow's HTTP client issues a `read()` per chunk, and for a non-blocking client each
*poll attempt* allocates the bounce buffer **before** the `EAGAIN` check — so a
busy read loop allocates one buffer per attempt even when no data is ready. Tens
of thousands of read attempts per second → millions of transient allocations.

**Why this matters for OOM (the direct link to the crash).** When meow passes a
large `count` (e.g. a 64 KB read buffer), `to_read = 64 KB` → a **16-contiguous-
page** allocation. That is exactly the kind of multi-page contiguous request that
`PmmOomHandler` cannot satisfy on a fragmented pool — and the crash dump shows the
heap jumping **+64 KB right before the abort** (`124 → 188 KB used`, peak
`181 → 222 KB`). The network bounce buffer is plausibly *both* the dominant
allocation churn *and* the trigger of the fragmentation abort.

**Mitigation (tracked, not yet done).** Replace the per-syscall `vec![0u8; count]`
with a small pool of reused, pre-allocated bounce buffers, or copy in ≤ 4 KB
chunks. Either shrinks the largest contiguous demand from 16 pages to 1, removing
most of the pressure that the heap-growth backoff and the planned OOM killer exist
to absorb. See `docs/OOM_KILLER_PLAN.md` → *Interaction with the network
bounce-buffer churn*.

### Earlier sweep (kept for history)

Verified with `scripts/test_memory_split.py` + the small-RAM sweeps in `logs/`
(tcc compiling `/akuma-playground/hello.c`):

**Measured `size`-profile sweep (June 2026, 883 KB binary, `tcc /akuma-playground/hello.c -o /tmp/hello`).** The action plan below (items 1–5) has since landed; these are the **post-fix** numbers:

| RAM | boots to SSH? | SSH usable? | runs tcc `hello.c`? | meow → ollama? | code+stack / heap / user / thread-limit | notes |
|---|---|---|---|---|---|---|
| 48 MB | yes | yes | **yes** | yes | 5 / 6 / 37 / 64 | — |
| 32 MB | yes | yes | **yes** | yes | 5 / 4 / 23 / 64 | was **no** (anon alloc OOM) before the fixes |
| 24 MB | yes | yes | **yes** | yes | 5 / 4 / 15 / 40 | was **no** (ELF load OOM) before |
| 16 MB | yes | yes | **yes** | yes | 5 / 4 / 7 / 14 | was **no** (SSH rejected, "memory low") before |
| 12 MB | yes | yes | **yes** | yes | 3 / 3 / 4 / 14 | now fits (no whole-binary slurp) |
| **8 MB** | **yes** | **yes** | **yes (repeatable)** | yes | 4 / 1 / ~2.08 / 14 | **`tcc hello.c -o /tmp/h && /tmp/h` compiles + runs repeatedly** (6 cycles verified, 2026-06-04). Earlier "marginal / `memory full`" was the dormant-cfg slurp bug (akuma-exec had no build.rs → `HEAP_SLURP_MAX` was 1 MiB, so the 723 KB tcc binary was slurped whole). Fixed by `crates/akuma-exec/build.rs` + heap→PMM reclaim — see *`tcc hello.c` now runs repeatedly at 8 MB* below |
| **7 MB** *(extreme)* | yes | yes | **yes** | **yes** | — (low-water 3 MB free) | direct `tcc hello.c -o out` + run prints `Hello, Akuma!`; **meow→tcc compile+run also works** here; `meow -c` → ollama streams a full reply (25.2 TPS). See *Extreme-profile compile floor* below |
| **6 MB** *(extreme)* | yes | yes | **yes** | **yes** | — (low-water 1 MB free) | direct `tcc` compiles+runs; **meow→ollama streams AND the meow→tcc *agentic* compile+run path now works** when the prompt forces one-command-at-a-time shell calls (verified `6mb_meow8.log`, 2026-06-05, 807 KB kernel). Was previously documented as OOMing. See *meow→tcc agentic path at 6 MB* below |
| **5 MB** *(extreme)* | yes | yes | no (`memory full`) | no | — (low-water 1 MB free) | **Superseded — see the milestone above.** Measured with the old non-static apk tcc; with the optimized `tcc -static` the compile-alone floor is now **4.5 MB** and the meow→tcc agentic floor is **5 MB**. **4 MB still does not boot** |

> Rows **≤ 7 MB** are `extreme`-profile (lighter reserve than `size`); the rows ≥ 8 MB
> are the `size`-profile sweep. See *Extreme-profile compile floor (re-measured
> 2026-06-05)* for the clean numbers and failure modes.

The **meow → ollama** column is directly verified at **6 MB** and **7 MB**
(`extreme`) and 256 MB; the higher `size` tiers are inferred (meow's ~1 MB working
set is smaller than tcc's ~1.5–2 MB, which is measured at each tier, and the
segment-boundary fix is RAM-independent). meow needs less RAM than tcc, so its
floor tracks the boot/SSH floor (~6 MB) rather than the tcc floor.

So on the `size` profile after the fixes: **boot/usable-SSH floor 6 MB, tcc floor
8 MB** (tcc down from 48 MB). Probed 2026-06-04: 6 MB and 7 MB both boot to a usable
SSH, but **tcc needs 8 MB** — at 7 MB user pages are only ~1 MB, below tcc's ~1.5–2 MB
working set, so a clean compile SIGSEGVs (`anon alloc failed, 0 free pages`). 5 MB does
not boot (the layout guard trips with 0 user pages). Pushing tcc below 8 MB would mean
clawing user pages out of the fixed ~4.92 MB `code+stack` reserve (1 MB heap seed / 1 MB
boot-stack guard / 2 MB pre-kernel region) — a layout change, not a loader one.

> Gotcha while probing: `/tmp` is **ext2-backed and persists across reboots**, so a
> `/tmp/h` left by a successful higher-RAM run masks a failed low-RAM compile (the old
> binary still runs `Hello, Akuma!`). Always `rm /tmp/h` before testing a new floor. For reference, the **pre-fix** floors were boot 16 MB /
usable-SSH 24 MB / tcc 48 MB, with `code+stack` a flat **11 MB** at every size.
(`tcc -run` is separately broken at *all* sizes — `runmain.o not found`, a TCC
runtime-install issue, not RAM; use `tcc … -o out` then exec `out`.) On the `release`
profile tcc is verified at ≥ 256 MB via `scripts/test_memory_split.py`.

What moved the floor: four over-provisioned "≥ 256 MB" reservations were cut on the
`size` profile — the 8 MB **eager** user stack → 128 KB (item 1), the 8 MB heap floor
→ 4 MB (item 5), 128 KB → 64 KB per-user-thread kernel stacks (item 3), and most
decisively the **`code+stack` region 11 MB → 5 MB** by removing the stale 8 MB
boot-stack gap (item 4). `code+stack` is now a flat ~5 MB instead of 11 MB. The
`PmmOomHandler` then dropped the heap seed to 1 MB (was 4 MB), unlocking SSH at 8 MB.

## meow → ollama runs at 7 MB — lazy-ELF segment-boundary zeroing (FIXED 2026-06-05)

**Symptom.** On the `size`/`extreme` kernel `meow -c "…"` failed immediately with
`Failed to create request buffer` at every RAM size — even 256 MB — while the
**byte-identical** `meow` binary worked fine on the `release` kernel. The error
came from `libakuma::open`, which builds a NUL-terminated path with
`format!("{}\0", path)` then calls `openat`. A userspace probe showed the
formatted `String` was **empty** (`as_ptr() == 0x1`, the dangling empty-`Vec`
pointer), so the kernel received path pointer `0x1` and returned `EFAULT` on
every `open`/`mkdir`/`unlink` (`[EFAULT] nr=56 … args=[…, 0x1, …]`).

**Root cause — the on-demand ELF loader zeroed a neighbour segment's `.rodata`.**
`release` slurps the whole ELF and copies every PT_LOAD segment into its pages
eagerly (`load_elf`, de-duplicating shared frames via `mapped_pages`). The
`size`/`extreme` profiles demand-page instead (`load_elf_from_path` →
`DeferredLazySegment`, faulted in by `exceptions.rs`). The demand-pager fills a
faulting page from the file **only up to the faulting segment's `filesz`** and
then **zero-fills the rest of the page**. When two PT_LOAD segments share a
boundary page — the normal `.text`→`.rodata` case — this clobbers the second
segment's file-backed bytes, and that segment's own lazy region then finds the
page already mapped and never fills it.

meow's layout (3 PT_LOAD segments) hits exactly this:

| Segment | flags | virtual range | shared page |
|---------|-------|---------------|-------------|
| 1 | R-X (text + early rodata) | `0x400000` … ends `0x4375D8` | `0x437000` |
| 2 | R-- (`.rodata`) | starts `0x437558` … `0x443274` | `0x437000` |
| 3 | R-W (`.data`/`.bss`) | `0x444000` … (page-aligned) | — |

Segments 1 and 2 share page `0x437000`. The first instruction fault read-aheads
through segment 1, maps `0x437000`, fills `[0x437000, 0x4375D8)` from the file
and **zeroes `[0x4375D8, 0x438000)`** — which is segment 2's `.rodata`. The ~2.6 KB
zeroed there held string literals and `format!` argument *pieces*
(`"https://"`, `"base_url"`, the template fragments). With the pieces zeroed,
`format!` writes nothing and returns an empty `String` → path pointer `0x1` →
`EFAULT`. Every userspace Rust ELF with a shared text/rodata boundary page was
silently corrupted on these profiles (paws, dash, herd, quickjs, …), not just
meow.

**Fix.** `boundary_extended_filesz()` in `crates/akuma-exec/src/elf/mod.rs`: when
a deferred segment's file data ends mid-page and the next PT_LOAD segment begins
in that same page, extend this segment's fill to the shared-page end. ELF
`p_offset`s are contiguous, so the next segment's bytes sit right after this
one's in the file. The extension is **bounded by the next segment's own file
extent**, so a final segment's trailing `.bss` is still zeroed (never filled
with file garbage). Covered by host unit tests in the `boundary_tests` module
(`cargo test -p akuma-exec boundary`). `release` (eager `load_elf`) is
unaffected.

**Verified (2026-06-05).** On the `extreme` kernel at **7 MB**: `meow -c` connects
to ollama (`connect(10.0.2.2:11434) = OK`), streams a full response (25.2 TPS,
~3.8 MB free), zero `path=0x1` faults. Also verified on `size`/`extreme` at
256 MB. Pre-fix it failed at all sizes on these profiles.

## Heap retention after running meow — kernel-heap growth + per-run creep (OPEN, observed 2026-06-05)

Observed on the **`extreme`** kernel at **7 MB** after running meow in prompt
mode (`meow -c "…"`). Two separate effects, both leaving less free RAM than a
fresh boot:

**1. One-time kernel-heap growth (~256 KB), not returned to PMM.** A single
agentic session (multiple ollama round-trips: ~17 KB×2 TLS record buffers,
growing conversation history, JSON request/response) needs more than the 896 KB
heap seed, so the allocator claims more pages from the PMM. After meow exits this
growth is **not** released back to the free pool:

| | total `free` (Mem) | Heap total | Heap used | Heap free |
|---|---|---|---|---|
| fresh boot (idle) | 3804 KB | 896 KB | ~195 KB | ~700 KB |
| after one `meow -c "…compile+run…"` | **3548 KB** (−256) | **1152 KB** (+256) | ~962 KB | ~189 KB |

`ps` reports **no processes running** afterwards, so this is retained kernel-side,
not a live process. (Matches the user-reported `131 KB → 954 KB` heap-used jump.)

**2. Per-run heap-used creep (~17–50 KB/run), not freed on process exit.** Once
the heap has grown to 1152 KB, further runs do **not** grow it again (Mem `free`
stays 3548 KB), but each `meow -c "hi"` leaves more heap *used* behind:

| run | Heap used | Heap free |
|---|---|---|
| idle after 1st (heavy) run | 962 KB | 189 KB |
| after `meow -c "hi"` | 1011 KB (+49) | 140 KB |
| after `meow -c "hi again"` | 1028 KB (+17) | 123 KB |

This is a slow **kernel-heap leak** in the process spawn/teardown path: heap
`used` does not return to the idle baseline (~195 KB) even with zero processes
running. The one-time +256 KB growth is benign on its own (peak demand), but the
creep will eventually force another PMM claim — or, at 7 MB, OOM — if a long-lived
box runs meow repeatedly. The `allocator::reclaim_to_pmm()` path (see *Heap→PMM
reclaim*) cannot return the grown spans because they are never fully free.

**Partially root-caused.** The dominant one-time growth was the **ext2 block
cache** — `BTreeMap<u32, Vec<u8>>` (cap 512). During a meow+tcc agentic run,
~228 blocks (one per 1 KB block read) were inserted as individual heap Vecs
scattered across the PMM-claimed 256 KB span. A single surviving entry pinned
the entire span, so `reclaim_to_pmm()` could never return it. This also caused
the SSH read-gap spikes (~1.6 s) seen in `logs/4.5mb_meow5.log`: allocations
that couldn't fit in the fragmented span triggered slow reclaim cycles. **Fixed**
(June 2026) — see *ext2 block-cache fix* below; the heap span now frees cleanly
after a run on the `extreme` profile.

Remaining candidates for the per-run creep: per-process teardown not freeing all
kernel-side structures (address-space / fd / signal tables), or VFS/socket buffers
held past close. Reproduce with the before/after `free` above; `[Mem]` serial
lines report live heap used/peak and cumulative `Allocs`.

## ext2 block-cache fix — ring buffer + extreme no-cache (June 2026)

**Root cause.** The ext2 block cache (`crates/akuma-ext2/src/ext2.rs`) was a
`BTreeMap<u32, Vec<u8>>` capped at 512 entries. Each entry was a **separate
heap allocation** (`Vec<u8>` of one ext2 block = 1 KB). After a meow+tcc agentic
run, ~228 block entries had been inserted and never fully evicted — each one a
live allocation scattered across the Talc PMM-claimed 256 KB span. As long as any
single `Vec<u8>` in that span survived, `reclaim_to_pmm()` could not return the
span to the PMM, and the heap grew permanently.

This was also the root cause of the **SSH read-gap spikes** (~1.6 s) seen in
`logs/4.5mb_meow5.log`: allocations that didn't fit in the fragmented span
triggered slow Talc reclaim cycles, stalling the SSH thread.

**Fix on `extreme` profile.** The cache is gated out entirely via
`#[cfg(not(kernel_profile_extreme))]`. All block reads go directly to disk, no
heap allocation at all. Zero heap impact at runtime.

**Fix on `size` and `release` profiles.** Replaced `BTreeMap<u32, Vec<u8>>` with
`BlockRingCache`:
- **64-entry ring** (down from 512-entry BTreeMap cap).
- **Single contiguous `Vec<u8>` backing** of `64 × block_size` bytes, allocated
  once at mount time (typically `64 × 1024 = 64 KB`). Tag array is `[u32; 64]`
  on the stack.
- **Dedup in `insert`**: if the block tag is already present, the insert is a
  no-op — prevents two threads racing on the same cold block from writing two
  stale copies into successive ring slots (which old `remove` would only clear
  the first of).
- Because the backing is one allocation, Talc can reclaim the span as a unit
  once the `Ext2Fs` is dropped — no fragmentation.

**Infrastructure.** `crates/akuma-ext2/build.rs` was added (mirrors `akuma-exec`
and `akuma-net`): emits `kernel_profile_size`/`kernel_profile_extreme` from
`OPT_LEVEL` + `CARGO_FEATURE_EXTREME`. `akuma-ext2/extreme` feature wired through
root `Cargo.toml` (`extreme = ["akuma-exec/extreme", "akuma-ext2/extreme"]`).

**Verified.** 47 crate tests pass. Both `size` and `extreme` profiles build and
boot; `busybox echo hello` (binary loaded from ext2) works on both.

## Extreme-profile compile floor (re-measured 2026-06-05)

Clean serial measurement on the **`extreme`** kernel (one VM at a time, no
contention; `SNAPSHOT=1` so `/tmp` is pristine each boot — no stale-binary
masking). The current `/usr/bin/tcc` is **`libtcc.so`-based** (loaded via
lazy-file `mmap`), much lighter resident than the static tcc the `size`-profile
"8 MB" figure was measured against — so the floor is lower here.

**Deterministic `/usr/bin/tcc /akuma-playground/hello.c -o /tmp/d` then run `/tmp/d`:**

| RAM | compile + run | free low-water during compile | failure mode |
|---|---|---|---|
| 8 MB | ✅ `Hello, Akuma!` | 4 MB | — |
| 7 MB | ✅ `Hello, Akuma!` | 3 MB | — |
| 6 MB | ✅ `Hello, Akuma!` | 2 MB | — |
| 5 MB | ❌ | 1 MB | tcc prints `memory full`, exit 1, no binary (tcc's own allocator, not a kernel OOM) |
| 4 MB | ❌ boots | — | tcc needs ~6 MB working set; kernel itself boots fine (664 PMM free pg) |

- **Direct tcc compile+run floor: 6 MB.** No kernel OOM markers at 6–8 MB.
- **Boot + usable-SSH floor: 4 MB** — see *Breaking the 4 MB boot wall* below. The
  earlier QEMU `Not enough space for DTB` barrier at 4 MB was broken by changing
  `text_offset` to 1 MB in the ARM64 Image header (kernel now loads at RAM_BASE + 1 MB;
  DTB lands at `0x40200000` which fits in 4 MB RAM).
- **meow→tcc agentic floor (`meow -c "compile … and run it"`): 6 MB** (with the
  one-command-at-a-time prompt below — was 7 MB). Verified working at 7 MB
  (`7mb_meow0.log`) and now at **6 MB** (`6mb_meow8.log`, 2026-06-05, 807 KB
  kernel): tcc spawns via `libtcc.so`, the binary is produced and run, no kernel
  OOM. The earlier "6 MB agentic path OOMs" finding held for a prompt that let the
  model combine compile-and-run into one `tcc … && /tmp/h` shell command (compile
  peak *plus* the run overlap meow's resident set > 6 MB free). Forcing the model
  to issue **separate sequential shell tool-calls** keeps the peak under the
  ceiling — see *meow→tcc agentic path at 6 MB* below.

Two gotchas that cost time here, recorded so they don't again:
- **`ls <file>` is unreliable on akuma** — listing a *file* path returns
  `Error listing directory: Not found` even when the file exists and runs.
  Verify a produced binary by **executing it**, not by `ls`.
- The agentic prompt's success is also gated by the **local model** reliably
  emitting the tcc tool-call (the request goes to ollama on the host) — this is
  independent of VM RAM. Drive these tests **serially**; concurrent meow→ollama
  sessions starve each other and produce degraded completions that look like the
  model "refusing" to call tools.

## meow→tcc agentic path at 6 MB (verified 2026-06-05)

The full **agentic** compile-and-run pipeline — meow drives the local model, which
emits tool-calls that spawn tcc and then run the output — now completes on the
**`extreme`** kernel at **6 MB**, where it was previously documented as OOMing. The
verified prompt (`6mb_meow8.log`, 807 KB kernel):

```
meow -c "compile /akuma-playground/hello.c with /usr/bin/tcc, put binary in /tmp/h6mb and run it, run commands one by one using shell tool"
```

**Why the prompt phrasing matters.** The decisive clause is *"run commands one by
one using shell tool"*. It steers the model to emit **separate sequential shell
tool-calls** (one to compile, a later one to run `/tmp/h6mb`) instead of a single
combined `tcc … && /tmp/h6mb` line. With sequential calls the tcc compile peak and
the binary-run never overlap, and meow's per-call working set is released between
round-trips, so the agentic peak stays under the ~3.3 MB of free PMM at 6 MB. The
earlier "6 MB agentic path OOMs" result came from a prompt that combined the two
steps — that overlap, on top of meow's resident set, exceeded 6 MB free.

**What the boot layout looks like at 6 MB extreme** (from the log header):

```
Code+Stack: 3 MB (0x40000000 - 0x403ec000) [stack-cover+1MB guard]
Heap:       0 MB (0x403ec000 - 0x404ac000) [~768 KB seed, auto-grows]
User pages: 1 MB (0x404ac000 - 0x40600000) [remaining]
```

After the PMM reclaims the 2 MB pre-kernel region the real free pool is **847 free
pages ≈ 3.3 MB** (`[Mem]` rounds it to `2/6MB free`). The `extreme` profile's 3 MB
`code+stack` (vs the `size` profile's 5 MB) and the 807 KB kernel are what free up
enough PMM for the agentic peak to fit.

**Trace through the log** (no `panic`, no `memory full`, no `0 free pages`, no
SIGSEGV anywhere in 685 lines; `panic=0` throughout):

1. meow (`PID 1`) connects to ollama (`connect 10.0.2.2:11434 = OK`), runs an LLM
   exchange (`[PSTATS] PID 1 /bin/meow … 503 syscalls, recvfrom=200, mkdirat=1,
   openat=10`).
2. tcc spawns as **`pid=2`** via lazy-file `mmap` of `/usr/lib/libtcc.so`, does the
   compile (heavy `mmap`/`munmap` churn), then exits cleanly (`Thread 9 recycled`).
   A handful of benign `[EFAULT] nr=63 … args=[0x3,0x0,0x0,…]` (read with a NULL
   buffer) appear during the compile — non-fatal, tcc continues.
3. Free dips to its low-water **`1/6MB`** at the compile peak; kernel heap grows
   `~768 KB → 1 MB` (used peaked **1038 KB**), then `reclaimed=256KB`→`512KB`.
4. **`pid=3`** and **`pid=4`** are tiny static binaries (identical layout
   `0x431000` / fault `0x410958`, no `libtcc.so`) spawned *after* tcc exits — i.e.
   `/tmp/h6mb` being executed — each preceded by a fresh meow→ollama call (the
   "one by one" steps).
5. The box settles back to `2/6MB free` and stays idle and healthy.

**Caveats.**
- It's tight: the compile dips the free pool to ~1 MB. There is no headroom for a
  second concurrent workload.
- The same **kernel-heap retention** seen at 7 MB applies (heap grows ~256 KB and
  is not fully returned to PMM after meow exits — see *Heap retention after running
  meow*). At 6 MB this is the closest margin; repeated agentic runs on a long-lived
  6 MB box could still OOM as the heap creeps.
- Success is **reflected in the log** — the `pid=3`/`pid=4` spawns of the small
  static binary (layout `0x431000` / fault `0x410958`, no `libtcc.so`) *are*
  `/tmp/h6mb` executing — **and was independently confirmed by running the produced
  `/tmp/h6mb` binary directly** (it executes and prints its output). The binary's
  stdout flows over SSH to meow rather than to serial, so the serial log shows the
  spawn/run events but not the program's printed text. It is also gated by the
  local model reliably emitting the sequential tool-calls (host-side,
  RAM-independent; drive serially).

**The final frontier — fail *gracefully* under OOM in sequential tool calls.**
6 MB works *when the run stays under the ceiling*, but the margin is ~1 MB and the
kernel-heap creep means a long sequence of agentic tool-calls will eventually
exhaust PMM on a 6 MB (and marginally 7–8 MB) box. The goal is no longer "never
OOM" — it is that when a spawn/compile *does* run out of memory mid-sequence, it
must **fail gracefully**: the failing `tcc`/spawn should return a clean `ENOMEM` to
meow (which already surfaces it as a tool error, e.g. "not found?"/non-zero exit),
the kernel must **not** panic or take down the whole VM, and the box must stay on a
usable SSH session so the agent can retry or report. Today some low-PMM paths still
degrade poorly (a heap slurp landing in a kernel thread with no current process can
`alloc_error_handler`-panic the kernel — see *Root cause A*; and the unbounded
heap creep has no back-pressure). Closing this means: bound/back-pressure the
kernel-heap growth so a single tool-call can't ratchet the pool down permanently,
make every spawn/`mmap`/page-fault OOM return `ENOMEM` to userspace instead of
panicking, and ensure per-process teardown fully returns memory so the *next*
sequential tool-call starts from a clean baseline. That turns 6 MB from "works if
you're lucky" into "works, or fails cleanly and keeps the box alive."

## What landed in the `even-smaller-kernel` branch (June 2026)

All changes are gated on `kernel_profile_size` (emitted by `build.rs` when `OPT_LEVEL=z`),
so `--release` is unaffected unless stated.

| Change | File | Before | After |
|--------|------|--------|-------|
| **Heap seed (`SMALL_FLOOR`)** | `src/main.rs` | 8 MB (both profiles) | 1 MB (`size`), 4 MB (`release`) |
| **`MIN_CODE_AND_STACK`** | `src/main.rs` | 8 MB | 4 MB |
| **`STACK_BOTTOM`** per-profile | `src/main.rs` | hardcoded `0x4090_0000` | `0x402E_C000` (`size`, see image-reserve note) / `0x4050_0000` (`release`) |
| **`IMAGE_SIZE`** (boot-stack reserve) | `src/boot.rs` + `build.rs` | `0x10_0000` (1 MB, `size`) | `0xEC000` (944 KB, page-aligned) — hand-tightened to ~30 KB above the ~914 KB kernel; reclaims **80 KB** to user pages |
| **Boot stack address in threading** | `ExecConfig` fields in `crates/akuma-exec/src/runtime.rs` | hardcoded `0x40700000` in threading crate | passed from `main.rs` as `boot_stack_base/top` |
| **`USER_STACK_SIZE_OVERRIDE`** | `src/config.rs` | 8 MB (all profiles) | 0 / auto-scale (128 KB at ≤256 MB) |
| **`SYSTEM_THREAD_STACK_SIZE`** | `src/config.rs` | 256 KB (all profiles) | 64 KB (`release`), 128 KB (`size`) |
| **`USER_THREAD_STACK_SIZE`** | `src/config.rs` | 128 KB (all profiles) | 64 KB (`size`), 128 KB (`release`) |
| **`compute_thread_limit` floor** | `src/main.rs` | `reserved + 2` | `reserved + 6` |
| **Demand-paged ELF loader** | `crates/akuma-exec/src/process/spawn.rs` | `HEAP_SLURP_MAX = 1 MB` | 0 on `size` profile — every binary uses `from_elf_path`; heap never needs a scratch buffer sized to the binary (tcc is 723 KB) |
| **Test modules excluded from binary** | `src/main.rs` | test modules always compiled in | all `*_tests` modules gated on `#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]` |
| **Interpreter loaded page-by-page** | `crates/akuma-exec/src/elf/mod.rs` | `read_file(ld-musl)` → ~600 KB heap slurp | `load_interpreter_from_path`: reads each PT_LOAD page with a 4 KB `file_read_exact` scratch buffer; peak heap use < 10 KB |
| **`do_execve` heap slurp** | `src/syscall/proc.rs` | `read_file(binary)` for every `execve` (e.g. linker is ~700 KB) | reads only first 256 bytes (shebang probe); always uses `replace_image_from_path` |
| **Lazy file-backed `mmap`** (`MMAP_FILE_BACKED_LAZY`) | `src/syscall/mem.rs` + `src/config.rs` | every file-backed `mmap` eagerly allocates all PMM frames | creates `LazySource::File` deferred region; pages faulted in on first access via existing demand-paging handler |
| **Debug instrumentation gated off on extreme** | `src/config.rs` | `PROCESS_SYSCALL_STATS`, `PROC_SYSCALL_LOG_ENABLED`, `PROC_SYSVIPC_ENABLED`, `SYSCALL_ERRNO_DIAG_EXTRA`, `DEBUG_SIGSEGV_SYSCALL_STUB` all `true` (not profile-gated) | all `false` under `#[cfg(kernel_profile_extreme)]` — drops the per-process syscall ring-buffer heap + LTO-strips the disabled branches. **Extreme image 821 → 805 KB (−16 KB); +4 user pages at every RAM ≤ 16 MB** (e.g. 4.5 MB 523 → 527 free, 8 MB 1307 → 1311). Disabling the log also makes `handle_syscall` skip the per-syscall timing read |
| **`PROC_SYSCALL_LOG_MAX_ENTRIES`** (all profiles where the log is on) | `src/config.rs` | 500 (~16 KB/process ring buffer) | **64** (~2 KB/process) — 64 recent syscalls is enough to see the lead-up to a fault |

**Three heap-slurp sites that blocked 8 MB tcc.** With `HEAP_SLURP_MAX = 0`, tcc's
own ELF is demand-paged (no heap scratch). But running `tcc hello.c` involves three
more large allocations that each individually exceeded the 1 MB heap seed:

1. **Interpreter loading** (`load_elf_from_path`, lines ~245 and ~869): the call to
   `read_file(ld-musl-aarch64.so.1)` slurped ~600 KB into the heap regardless of
   `HEAP_SLURP_MAX`. Fixed by `load_interpreter_from_path` (page-by-page `read_at`).

2. **`execve` in the linker** (`do_execve`): tcc invokes `/usr/bin/ld` (~700 KB) via
   `execve`. `do_execve` called `read_file(binary)` before checking if the path-based
   loader was needed. Fixed: on `kernel_profile_size` reads only 256 bytes (shebang
   check) then falls through to `replace_image_from_path`.

3. **Shared-library mmap** (`sys_mmap`): ld-musl maps `libtcc.so` and other `.so`
   files via `mmap(fd, ...)`. File-backed mmaps were always eager — PMM exhausted
   before the process started. Fixed by `MMAP_FILE_BACKED_LAZY = true` on the `size`
   profile: file-backed mmaps now push a `LazySource::File` lazy region; the existing
   page-fault handler reads pages on demand.

**Result:** free memory at boot 716 KB → 2764 KB; `tcc hello.c -o /tmp/hello &&
/tmp/hello` prints `Hello, Akuma!` at 8 MB. The linker subprocess still OOMs during
the link step (heap fills to ~1.3 MB via `PmmOomHandler`) but produces the output
binary before dying — a separate issue from spawning and running tcc itself.

## `tcc hello.c` now runs repeatedly at 8 MB (FIXED 2026-06-04)

**`tcc /akuma-playground/hello.c -o /tmp/h && /tmp/h` now compiles + runs `Hello,
Akuma!` repeatedly at MEMORY=8M (size profile), no reboot between runs.** Verified
6 consecutive compile→run cycles, zero OOM, zero kernel faults. Two bugs were behind
the prior "first compile = `memory full`, repeats SIGSEGV, kernel eventually panics"
behaviour:

### Root cause A — the size-profile ELF gates in `akuma-exec` were DEAD CODE

`crates/akuma-exec` had **no `build.rs`**, so the `kernel_profile_size` cfg was never
emitted for that crate — yet it contains **7** `#[cfg(kernel_profile_size)]` gates. Every
one silently compiled the `not(kernel_profile_size)` branch *even on the size profile*.
So the headline "even-smaller-kernel" ELF fixes attributed to akuma-exec (demand-paged
loader threshold `HEAP_SLURP_MAX = 0`, page-by-page interpreter loader) **never actually
took effect** — the kernel still ran with `HEAP_SLURP_MAX = 1 MiB` and the slurp-based
interpreter loader. The bin crate and `akuma-net` each have a `build.rs` doing the
`OPT_LEVEL == "z"` detection; akuma-exec was simply missing one.

Consequence: spawning `/usr/bin/tcc` (723 480 bytes < 1 MiB) read the **entire binary**
into the kernel heap (`spawn.rs` `read_file` path), and the dynamic linker was slurped
too. That pushed the heap peak to ~2 MB on an 8 MB box, starving tcc's demand-paged
user pages → `memory full` / SIGSEGV. Under fragmentation a later 723 KB slurp landed in
a kernel thread with no current process, so `alloc_error_handler` had nothing to kill and
**panicked the whole kernel** (`EC=0x3c` BRK).

**Fix:** added `crates/akuma-exec/build.rs` (mirrors `akuma-net`) emitting
`kernel_profile_size` under `OPT_LEVEL=z`. All 7 gates now activate on the size profile:
no binary slurp, interpreter loaded page-by-page. Heap peak during a tcc compile dropped
~2005 KB → ~1583 KB and is stable across runs. `load_interpreter` (the slurp variant) is
now `#[cfg(not(kernel_profile_size))]`.

Also hardened `spawn.rs`: the loader choice no longer falls back to a whole-file
`read_file()` when `file_size()` returns `None` (a transient under memory pressure). On
the size profile it now *always* uses the demand-paged path loader and re-stats if needed
— so even a stat hiccup can't route a large binary into a heap slurp.

### Root cause B — kernel heap watermark was one-way (now reclaimed)

`PmmOomHandler` grows the heap by claiming ≥256 KB spans from the PMM (`alloc_pages_
contiguous_zeroed` → `talc.claim`). Talc never returned them, so the free PMM pool
ratcheted down after every memory-hungry process (e.g. meow's 8 TCP sockets × 32 KB
buffers) and a later spawn / page-fault hit `0 free pages`.

**Fix:** `allocator::reclaim_to_pmm()` (src/allocator.rs) records every PMM-backed span
and, for each one that is *entirely* free inside Talc, `truncate`s it out of the heap and
returns the pages to the PMM (talc 4.4.3's `get_allocated_span` + `truncate`). It is
called:
* from `pmm::alloc_page` / `alloc_pages_contiguous_zeroed` on allocation **failure**
  (reclaim-under-pressure, then retry once) — deadlock-safe via `TALC.try_lock()`, which
  is a no-op when reentered from inside `handle_oom`;
* periodically from the memory monitor (idle watermark trim; surfaced as
  `reclaimed=<N>KB` in the `[Mem]` line).

Gated boot self-test: `test_heap_reclaim_returns_pages_to_pmm` in `src/process_tests.rs`
forces a PMM claim, frees it, and asserts the pool returns to baseline (verified at
MEMORY=48M: "reclaimed 1542 pages, recovered to 7548 of 7548"). Skips on ≥256 MB boots
(heap seed too large to cheaply force a claim).

> For the `tcc hello.c` workload, fix A is what makes repeats work — the heap peak now
> stays ~1.5 MB and the ~2 MB user pool returns cleanly between runs, so no claimed span
> is ever fully free for B to reclaim. Fix B is the safety net for socket-heavy workloads
> (meow) where the heap genuinely frees large buffers on exit.

### 2. meow spawn — NOT a regression; it was the issue-#1 OOM (resolved 2026-06-04)

**Status: meow's Shell-tool spawn works at 8 MB (`size`) and 16 MB (`release`).**
Verified 2026-06-04 with `meow -c "execute /tmp/hello"` against host Ollama
(`qwen3-yolo:latest` at `10.0.2.2:11434`): the model emits a `Shell` tool call, meow's
`tool_shell` (`userspace/meow/src/tools/shell.rs`) spawns the child via the **SPAWN
syscall #301** (`spawn_process_with_channel_cwd`, *not* fork+`execve`), drains its stdout
pipe, observes exit, and reports `Exit code: 0` / `Tool Status: Success`. Kernel PSTATS
for the meow PID confirms the spawn succeeded:

```
[PSTATS] PID 3 (/bin/meow) ... nr301=1(4ms) ... nr303=20(0ms) ...
```

`nr301` = SPAWN #301 (one call, 4 ms, no `ENOMEM`); `nr303` = the child-stdout drain
reads. Output captured at 8 MB (size profile): `hello (1/10) … (10/10) … Exit code: 0`.

**The earlier "Failed to spawn '…' (not found?)" was issue #1, not a `do_execve`
regression.** meow's Shell tool does **not** go through `do_execve` — it uses SPAWN #301,
which calls `spawn_process_with_channel_cwd` directly. The "not found?" message is what
`tool_shell` prints whenever `libakuma::spawn()` returns `None`, and `spawn()` returns
`None` for **any** negative syscall result — including `ENOMEM` (`src/syscall/proc.rs::
sys_spawn` returns `ENOMEM` when the spawn fails for lack of PMM pages). So the prior
failure was a low-PMM spawn after the heap watermark had eaten the free pool (issue #1
above), surfaced through a generic error string — not the demand-paged ELF loader. On a
reasonably-fresh boot with PMM available, the spawn succeeds.

**Caveat:** this is still gated by issue #1. After enough socket/heap churn (e.g. several
meow LLM round-trips on an 8 MB box) the PMM pool can drop low enough that a *subsequent*
SPAWN returns `ENOMEM` and meow again prints "not found?". The fix is the issue-#1
PMM-reclaim work, not anything in the spawn/exec path.

**Note on the release profile at 8 MB:** `--release` does not boot at 8 MB at all — the
3 MB image reserve makes `code+stack` 7 MB, leaving **0 user pages**, so the layout guard
in `src/main.rs` halts with `FATAL: kernel memory layout invalid`. For ≤ 8 MB build the
`size` profile (`scripts/build_size.sh`); release's boot floor is higher (see the
boot-floor tables above).

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
`0x40500000–0x40600000` on `release` and `0x402EC000–0x403EC000` on `size` (944 KB
image reserve). This
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
  (size `0x403EC000` / release `0x40600000`), so an early
  exception can't scribble into the heap either.

**Verification.** `release` now boots to SSH at 16/24/32 MB (was: hang at all of them);
`tcc /akuma-playground/hello.c` compiles + runs at release@32 → "Hello, Akuma!";
`size` still boots at 16 MB (no regression). Release boot floor dropped 128 MB → ≤ 16
MB.

## Tightening the boot-stack reserve (size profile, June 2026)

The boot stack is placed immediately above the kernel image at
`STACK_BOTTOM = KERNEL_PHYS_LOAD + IMAGE_SIZE`. The `size` profile reserved a flat
**1 MB** `IMAGE_SIZE` even though the kernel is only ~914 KB
(`_kernel_phys_end = 0x402E4650`), so ~110 KB of slack sat between the binary and the
boot stack — and because `IMAGE_SIZE` is fixed, kernel growth (and the neko-editor
feature drop) was *absorbed by that slack* rather than handed back to user pages.

**Change:** hand-tighten the `size`-profile `IMAGE_SIZE` from `0x10_0000` (1 MB) to
`0xEC000` (**944 KB**, page-aligned — ~30 KB over the current kernel). This is a single
value mirrored across **five** locations that must stay in lockstep (the long-standing
footgun behind the earlier heap/boot-stack overlap crash):

| Location | Symbol | Value |
|---|---|---|
| `src/boot.rs` | `IMAGE_SIZE` (size) → ARM64 header + `BOOT_STACK_TOP` (SP) | `0xEC000` |
| `build.rs` | `image_size` → `--defsym=STACK_BOTTOM` (linker `ASSERT`) | `0xEC000` |
| `src/main.rs` | `STACK_BOTTOM` const (size) | `0x402E_C000` |
| `src/main.rs` | `BOOT_STACK_TOP` = `STACK_BOTTOM + 1 MB` | `0x403E_C000` |
| `src/exceptions.rs` | boot-thread exception stack top (size) | `0x403E_C000` |

The linker `ASSERT(_kernel_phys_end < STACK_BOTTOM)` in `linker.ld` fails the build if
the kernel ever outgrows the 944 KB reserve — bump the value then.

**Measured at `MEMORY=8` (size profile, neko off, 940 KB binary):**

| | 1 MB reserve | 944 KB reserve | Δ |
|---|---|---|---|
| `code+stack` region | `0x40000000–0x40500000` (5 MB) | `0x40000000–0x404EC000` (~4.92 MB) | −80 KB |
| user pages | `0x40600000–0x40800000` (2.000 MB) | `0x405EC000–0x40800000` (2.078 MB) | **+80 KB** |
| `free` (Mem) | 2764 KB | **2844 KB** | **+80 KB** |

The whole layout shifts down exactly `0x14000` (80 KB) and that 80 KB lands entirely in
the user-page pool. The runtime layout guard in `main.rs` passes (it prints the expected
"within 4 MB of stack — 30 KB margin" advisory; that warning is informational, the
kernel binary is static). Boots cleanly to SSH; binary size is unchanged (the reserve is
RAM, not file).

**tcc status at 8 MB.** With the kernel now ~914 KB (grown from the 883 KB that first
verified 8 MB tcc), `tcc hello.c` is **OOM-marginal at 8 MB**: a fresh-boot first-spawn
reports `memory full`, and after other process spawns it SIGSEGVs on
`anon alloc failed, 0 free pages` (the documented "PMM pool not reclaimed after exit"
issue — see *Open issues*). The 80 KB reclaim helps but doesn't close the gap.
Verified compiling + running `hello.c` → "Hello, Akuma!" at **≥ 16 MB**. The reclaim is
strictly memory-positive, so it cannot regress tcc; closing the 8 MB tcc gap needs the
PMM-reclaim fix and/or further reservation cuts, not this knob.

**Done (June 2026) — the reservation is now exact, not hand-tuned.** The 944 KB
(and the later extreme 848 KB) manual high-water marks have been **replaced** by a
linker-derived reservation: `linker.ld` computes `STACK_BOTTOM =
ALIGN(_kernel_phys_end, 0x1000) + 0x2000`, `STACK_TOP = STACK_BOTTOM + 1 MB`, and
`IMAGE_RESERVE = STACK_BOTTOM - KERNEL_PHYS_BASE`, exporting them as absolute
symbols. `boot.rs` asm loads `STACK_TOP` (initial SP) and emits `IMAGE_RESERVE`
(Image header); `main.rs`/`exceptions.rs` read `STACK_BOTTOM`/`STACK_TOP` as
`extern` symbols; `build.rs` no longer injects `--defsym=STACK_BOTTOM` or any
`IMAGE_SIZE`. This auto-tracks the binary on every build and collapses the
five-location sync into **one source of truth** (linker.ld). See *Dynamic
boot-stack reservation* below for the resulting numbers; the boot self-test
`test_boot_stack_reservation_invariants` guards the invariants.

## Tightening the extreme-profile reserve via the RSA feature gate (June 2026)

The same `IMAGE_SIZE`-tightening lever as the `size`-profile section above,
applied to `extreme` after shrinking the kernel. RSA TLS-cert verification is now
behind the `tls-rsa` Cargo feature (on by default, **off in `size`/`extreme`** —
both build `--no-default-features`). SSH is Ed25519-only and never used RSA; the
only consumer was outbound-HTTPS server-cert verification, where ECDSA/Ed25519
stay available. Full design + size breakdown: **`docs/RSA_FEATURE_GATE.md`**.

Dropping `rsa` (and its `num-bigint-dig` bignum code) shrank the `extreme` flat
`.bin` from 807 KB → 776 KB (and `_kernel_phys_end` 853 KB → 821 KB incl. `.bss`).
The image saving alone frees **no** RAM — the boot stack sits at
`STACK_BOTTOM = KERNEL_PHYS_LOAD + IMAGE_SIZE`, and both images fit the old
reserve. To hand the slack back, the reservation has to shrink with the binary.

This was first done by hand-tightening the `extreme` `IMAGE_SIZE` 880 KB → 848 KB
(a 3-way-lockstep constant), then **superseded** by deriving the reservation in
`linker.ld` (see *Dynamic boot-stack reservation* below), which auto-tracked it
to **832 KB** — 16 KB tighter still, with no constant to maintain.

**Measured free memory, before (rsa-on, 880 KB reserve) vs after (rsa-off, dynamic
832 KB reserve), same disk snapshot.** The layout shifts down and the freed bytes
land entirely in the user-page pool:

| MEM | | Kernel binary | PMM free pages | `free` (Mem) |
|---|---|---|---|---|
| 8 MB | before | 853 KB | 1295 | 4/8 MB |
| 8 MB | hand-tuned 848 KB | 821 KB | 1303 | 4/8 MB |
| 8 MB | **dynamic 832 KB** | 821 KB | **1307** | 4/8 MB |
| 5 MB | before | 853 KB | 623 | 2/5 MB |
| 5 MB | **dynamic 832 KB** | 821 KB | **635** | 2/5 MB |

**+12 PMM pages = +48 KB of user-page pool at every RAM size** vs the original
rsa-on/880 KB kernel (+8 from rsa, +4 more from the dynamic-vs-848 tightening).
The `[Mem]` free RAM is MB-granular so the headline figure is unchanged; the gain
is real and shows in the page-granular `PMM stats`. Boot reaches `[SSH Server]
Listening` cleanly at 5/6/7/8 MB.

**Boot-to-SSH floor: 5 MB** (at time of RSA removal). 4/5/6/7 MB all boot to usable SSH
after the `text_offset` fix (see *Breaking the 4 MB boot wall* below). The RSA removal
alone doesn't change the floor; the `text_offset` change did. Logs: `logs/rsa-purge/`.

## Dynamic boot-stack reservation (June 2026)

The per-profile `IMAGE_SIZE` constant — and the 3-way (`build.rs` / `boot.rs` /
`main.rs`) lockstep footgun behind several past heap/boot-stack-overlap crashes —
is **gone**. The boot-stack reservation is now derived from the *actual* linked
image size in `linker.ld`, the one place that knows it (`build.rs` runs
pre-link), and exported as absolute symbols every other site reads:

```ld
_kernel_phys_end = .;                                      /* end of image incl. .bss */
STACK_BOTTOM  = ALIGN(_kernel_phys_end, 0x1000) + 0x2000;  /* page-align + 2-page guard */
STACK_TOP     = STACK_BOTTOM + 0x100000;                   /* 1 MB boot stack */
IMAGE_RESERVE = STACK_BOTTOM - KERNEL_PHYS_BASE;           /* ARM64 Image header field */
```

| Consumer | Was | Now |
|---|---|---|
| `src/boot.rs` asm SP | `ldr =STACK_TOP` from injected `BOOT_STACK_TOP` const | `ldr =STACK_TOP` (extern linker symbol) |
| `src/boot.rs` Image header | `.quad {image_size}` (per-profile `IMAGE_SIZE`) | `.quad IMAGE_RESERVE` (linker symbol) |
| `src/main.rs` overlap-halt + heap reserve + ExecConfig bounds | per-profile `STACK_BOTTOM`/`BOOT_STACK_TOP` consts | reads `STACK_BOTTOM`/`STACK_TOP` externs |
| `src/exceptions.rs` boot-thread exception stack | per-profile hardcoded top | reads `STACK_TOP` extern |
| `build.rs` | `--defsym=STACK_BOTTOM` + per-profile `image_size` | nothing (cfg emission only) |

Result: the reservation auto-tracks the binary on every build, with **one** source
of truth (`linker.ld`). Reclaimed automatically per profile — the boot
`Kernel binary:` / `PMM stats` lines confirm the values:

| profile | `_kernel_phys_end` | `IMAGE_RESERVE` (was) | reclaimed |
|---|---|---|---|
| `extreme` | 821 KB | 832 KB (was hand-tuned 848, orig 880) | +12 pages vs orig |
| `size` | 881 KB | 892 KB (was 944) | +13 pages |
| `release` | 2875 KB | 2884 KB (was 3072) | +47 pages |

Verified booting to SSH: `release` @ 64 MB (full boot self-test suite),
`size` @ 8 MB, `extreme` @ 8 MB (PMM 741 alloc / 1307 free) and @ 5 MB (645 /
635, floor holds). The boot self-test **`test_boot_stack_reservation_invariants`**
(`src/process_tests.rs`) asserts `STACK_BOTTOM > _kernel_phys_end`, page
alignment, the exact 1 MB stack, and a sane guard — so a future `linker.ld` edit
that breaks the derivation fails the boot suite instead of silently overlapping
the image or the heap.

### Full boot + hello matrix (June 2026, post-text_offset)

Every (profile, RAM) cell booted under QEMU `virt` with a fresh disk snapshot,
then the **hello probe** `busybox echo HELLO_AKUMA_OK` was run over SSH — a real
userspace process spawn (busybox-static via the demand-paged loader), so a ✅ means
the kernel booted to SSH *and* can load+run a userspace binary. Numbers are PMM
**free pages** at boot (×4 KB ≈ MB free).

Cells marked **†** were re-verified after the `text_offset = 1 MB` / `KERNEL_PHYS_BASE
= 0x40100000` change (June 2026). Unmarked cells are from the earlier
`scripts/boot_hello_matrix.py` sweep (`logs/rsa-purge/matrix_*.log`) and will differ
by ±30 pages at the low end (pre-kernel reclaim shifted from 2 MB to 1 MB, offset by
the 1 MB smaller `code+stack` region; net effect ≈ 0 but exact count varies by profile
and heap-seed rounding).

| RAM | release | size | extreme |
|-----|---------|------|---------|
| 4.0 MB | — | ✗ QEMU DTB | ✅ hello · 664 **†** |
| 4.5 MB | — | ✗ 0 user pages | ✅ hello · 527 |
| 5 MB | — | ✗ 0 user pages | ✅ hello · 639 |
| 6 MB | — | ✅ hello · 571 **†** | ✅ hello · 863 |
| 7 MB | — | ✅ hello · 828 | ✅ hello · 1087 |
| 8 MB | — | ✅ hello · 1019 **†** | ✅ hello · 1311 |
| 16 MB | ✅ hello · 1837 **†** | ✅ hello · 2844 | ✅ hello · 3103 |
| 32 MB | ✅ hello · 5933 **†** | ✅ hello · 6428 | ✅ hello · 6683 |
| 64 MB | ✅ hello · 13097 | ✅ hello · 13596 | ✅ hello · 13819 |
| 128 MB | ✅ hello · 27130 | ✅ hello · 27131 | ✅ hello · 27131 |
| 256 MB | ✅ hello · 45562 | ✅ hello · 45563 | ✅ hello · 45563 |
| 1024 MB | ✅ hello · 213498 | ✅ hello · 213499 | ✅ hello · 213499 |
| 4096 MB | ✅ hello · 918010 | ✅ hello · 918011 | ✅ hello · 918011 |

(release was swept 16 MB and up, per its boot floor; size/extreme add the 4–8 MB
low band.) Kernel images: release 2875 KB, size 881 KB, extreme **801 KB**. The
extreme column is the shipped kernel with debug instrumentation gated off (see the
debug-instrumentation row in *What landed in the even-smaller-kernel branch*);
each extreme cell ≤ 16 MB gained +4 pages vs the pre-gate sweep (the gate frees
16 KB, which lands in user pages while the heap is at its seed).

**Boot-to-hello floors: extreme 4.0 MB, size 6 MB, release ≤ 16 MB.** Reading the
failures bottom-up:

- **< 4.0 MB** — all profiles fail on QEMU `Not enough space for DTB after
  kernel/initrd`. The kernel now loads at +1 MB (`text_offset = 0x100000`), so
  DTB lands at `0x40200000`; below 4 MB even this doesn't fit.
- **4.0 MB — extreme boots and runs hello**: PMM 1024 total / 360 alloc / **664
  free**. `meow -c "say hi"` also connects to ollama and replies here
  (`logs/4mb_meow0.log`, 2026-06-05). This is the new extreme floor following the
  `text_offset` change (see *Breaking the 4 MB boot wall* below). The matrix
  above uses the RSA-off dynamic-reserve kernel (801 KB) built with that change.
- **4.5 MB — prior extreme floor** (before `text_offset` fix): PMM 1152 total /
  625 alloc / **527 free** (`logs/rsa-purge/ext_*k.log`). Now superseded.
- **5 MB — size still fails the kernel layout guard** (0 user pages): its larger
  881 KB image makes `code+stack` ~4.87 MB. **size needs 6 MB**; extreme runs at
  4.0 MB.
- **6 MB and up — all three (where they boot) run hello cleanly.**

Two things the matrix makes visible:
1. At small RAM the smaller kernel + tighter dynamic reserve wins monotonically:
   **extreme > size > release** free pages (e.g. 16 MB: 3103 vs 2844 vs 1837 **†**).
2. At **≥ 128 MB the three converge** to within one page of each other — the
   per-profile reservation is negligible against RAM, and `compute_heap_size()` /
   the thread-stack pool dominate. The profile choice only matters for the low-RAM
   band, which is exactly where the dynamic reserve was aimed.

### Workload floors on `extreme` — it's the *workload's* working set, not the kernel

Below ~6 MB the binding constraint is no longer the kernel: it's how much the
program itself faults in. Measured on the `extreme` kernel (free = PMM free pages
at boot, ≈ ×4 KB), one VM at a time so ollama isn't contended:

| Workload | Floor | At the floor | What gates it lower |
|---|---|---|---|
| **boot + SSH** | **4.0 MB** | 664 free pg (~2.6 MB) | QEMU DTB placement (< 4.0 MB doesn't fit) |
| **`busybox echo` (hello)** | **4.0 MB** | 664 free pg | same as boot |
| **`meow -c "say hi"`** | **4.0 MB** | low-water 1804 KB free; real model reply | one socket + HTTP + streamed reply; `logs/4mb_meow0.log` |
| **`tcc hello.c`** (apk / `libtcc.so`) | 6 MB | — | tcc's own working set: `libtcc.so` resident + compile buffers ≈ 3 MB |
| **`tcc -static hello.c`** | **4.5 MB** | — | static binary; no libtcc.so; floor unchanged from pre-`text_offset` era |
| **`meow` agentic (`tcc -static`)** | **4.5 MB** | 1988 KB low-water; 2520 KB settled | tcc-static is the bottleneck; SSH responsiveness degraded at this edge (`logs/4.5mb_meow5.log`) |

`meow -c "say hi"` was re-verified at **4.0 MB** (`logs/4mb_meow0.log`, 3 sessions,
`qwen3-yolo:latest`) after the `text_offset` change; previously verified at
**4.5 / 5.0 / 5.5 / 6.0 MB** (`logs/rsa-purge/meow_*k.log`). tcc fails below 6 MB
by exhausting the PMM pool while demand-paging `libtcc.so`: at 4.5 MB it SIGSEGVs
after ~12 syscalls (never starts compiling); at 5.5 MB it gets ~123 syscalls in
before `0 free pages`; 6 MB is the first size its full resident set fits. So the
earlier "meow needs ~6 MB" was the *agentic-compile* path (meow **+** tcc); a bare
LLM prompt now bottoms out at the kernel floor, 4.0 MB.

**Rule of thumb:** lightweight userspace (shell, a static hello, a single LLM
round-trip) runs at the 4.5 MB kernel floor; anything that demand-pages a large
shared library or holds multi-MB buffers (tcc, sustained agentic sessions) is
gated by its *own* footprint, around 6 MB.

## Breaking the 4 MB boot wall — ARM64 Image text_offset (June 2026)

**Previous state.** QEMU places the DTB at `ALIGN_UP(kernel_load + image_size, 2 MB)`.
With `text_offset = 0`, QEMU adds 2 MB automatically (`if text_offset < 4KB`), loading
the kernel at `RAM_BASE + 2 MB = 0x40200000`. At 4 MB RAM (ram_end `0x40400000`):
`DTB = ALIGN_UP(0x40200000 + ~0xCB000, 2MB) = 0x40400000`. QEMU's check
`dtb_start < ram_end` → `0x40400000 < 0x40400000` → **false** → `Not enough space for
DTB after kernel/initrd`. So the old boot floor was 4.5 MB even though the kernel only
occupies ~820 KB.

**Fix.** Set `text_offset = 0x100000` (1 MB) in `boot.rs` → QEMU uses it as-is (≥ 4 KB
threshold), loading the kernel at `RAM_BASE + 1 MB = 0x40100000`. Linker's
`KERNEL_PHYS_BASE` updated to match. New DTB location:
`ALIGN_UP(0x40100000 + ~0xCB000, 2MB) = 0x40200000`. Check: `0x40200000 < 0x40400000`
→ **true**. Boots at 4 MB.

| | Before (`text_offset = 0`) | After (`text_offset = 1 MB`) |
|---|---|---|
| Kernel load address | `0x40200000` (RAM_BASE + 2 MB) | `0x40100000` (RAM_BASE + 1 MB) |
| DTB placement (extreme ~820 KB) | `0x40400000` | `0x40200000` |
| QEMU check at 4 MB | ❌ `0x40400000 < 0x40400000` fails | ✅ `0x40200000 < 0x40400000` passes |
| Pre-kernel reclaim | 2 MB (512 pages) | 1 MB (256 pages) |
| `KERNEL_PHYS_BASE` | `0x4020_0000` | `0x4010_0000` |

**Files changed:**
- `src/boot.rs` — `.quad 0x100000` (was `.quad 0`)
- `linker.ld` — `KERNEL_PHYS_BASE = 0x40100000` (was `0x40200000`)
- `src/config.rs` — `pub const KERNEL_PHYS_BASE: usize = 0x4010_0000`; `pub const KERNEL_PHYS_OFFSET: usize = 0x10_0000`
- `src/main.rs` — uses `config::KERNEL_PHYS_BASE` / `config::KERNEL_PHYS_OFFSET`
- `src/exceptions.rs` — uses `crate::config::KERNEL_PHYS_BASE as u64`
- `crates/akuma-exec/src/threading/mod.rs` — local `const KERNEL_PHYS_BASE` for TTBR0 range checks

**Trade-off.** Pre-kernel reclaim drops from 2 MB to 1 MB (one fewer PMM page-spanning
reclaim). At 4 MB this is offset by the 1 MB smaller `code+stack` region (STACK_TOP is
1 MB lower), yielding **net +256 PMM pages** vs the old layout. The boot guard
`WARNING: Kernel is within 4MB of stack! (10 KB margin)` fires at 4 MB — the image is
tight, but the linker `ASSERT` still passes (`_kernel_phys_end < STACK_BOTTOM`).

**Verified** (`logs/4mb_meow0.log`, extreme 801 KB, 2026-06-05):
- Boots to SSH; PMM 1024 total / 360 alloc / **664 free**
- `meow -c "say hi"` connects to ollama, receives a reply; low-water 1804 KB free
- 3 independent meow sessions at 4 MB; zero crash markers; `panic=0` throughout

## Per-RAM memory statistics (June 2026)

Computed from the live heuristics in `src/main.rs` (`compute_heap_size`, `compute_thread_limit`)
and `src/config.rs` (`USER_THREAD_STACK_SIZE`, `USER_STACK_SIZE_OVERRIDE`).
Layout constants: size profile `stack_cover = 5 MB`; release profile `stack_cover = 7 MB`.
Thread pool comes from user pages (PMM), not the heap.

**size profile** — 883 KB binary, `USER_THREAD_STACK_SIZE` 64 KB, user-stack auto-scales (≤ 256 MB → 128 KB). Heap seed is now 1 MB (grows on demand via `PmmOomHandler`). **Note:** the table below predates the 944 KB image-reserve fix (`IMAGE_SIZE` 1 MB → 944 KB, see the *Tightening the boot-stack reserve* section); that fix shifts `code+stack` down ~80 KB (5 MB → ~4.92 MB) and adds the same 80 KB to every row's user-page / free-for-procs column:

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
| `tcc` multi-file C project (neatvi, 18 files) | — | **16 MB** (verified June 2026) |
| `apk search busybox` | 80 MB | 80 MB |
| `apk add busybox` | 80 MB | 80 MB (reliable: 96 MB) |

**neatvi compiled from source at 16 MB** (size profile, June 2026).
Source: https://github.com/aligrudi/neatvi (cloned to `/neatvi`):

```
tcc -I/neatvi /neatvi/cmd.c /neatvi/conf.c /neatvi/dir.c /neatvi/ex.c \
    /neatvi/lbuf.c /neatvi/led.c /neatvi/mot.c /neatvi/reg.c \
    /neatvi/regex.c /neatvi/ren.c /neatvi/rset.c /neatvi/rstr.c \
    /neatvi/sbuf.c /neatvi/syn.c /neatvi/tag.c /neatvi/term.c \
    /neatvi/uc.c /neatvi/vi.c -o /bin/vi
```

This is the first verified multi-file C compilation on Akuma — 18 translation units
compiled and linked in a single tcc invocation at 16 MB.

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

### Lazy thread-stack allocation (size profile, 2026-06-04)

`thread_limit` bounds how many slots *may* get a stack; it does **not** mean they're
all allocated up front. On **release** they still are (pre-allocated at boot, guaranteed
available, no per-spawn cost). On the **size profile** stacks are now lazy:

- **init** pre-allocates only a *warm floor* of FREE stacks per class —
  `WARM_FREE_SYSTEM = 1` + `WARM_FREE_USER = 1` (`crates/akuma-exec/src/threading/mod.rs`).
- **`ensure_slot_stack`** allocates a slot's stack on `claim` if absent; on a genuinely
  exhausted PMM the spawn fails cleanly (slot released, ENOMEM) instead of running on a
  null stack. (No-op on release — all slots are pre-allocated.)
- **`cleanup_terminated`** frees a recycled slot's stack back to the PMM *unless* that
  would drop the class below its warm floor. Safe point: the thread has terminated and
  cooled down, so it is no longer on its stack.

Why: at 8 MB, `thread_limit = 14` reserved **1280 KB** of PMM for stacks while only ~3
threads run at idle — ~1 MB sat idle. Lazy holds only `infra (2×128 KB) + warm floor
(128 KB + 64 KB) ≈ 448 KB` at idle and rents the rest while in use.

**Measured (size profile):**

| | idle RAM free | notes |
|---|---|---|
| 8 MB, pre-alloc (before) | 2 / 8 MB | — |
| 8 MB, lazy (after) | **3 / 8 MB** | **+1 MB at idle**; tcc still compiles+runs ×5; 34 spawn/recycle cycles, 0 canary faults, 0 spawn failures |
| 6 MB, lazy | 1 / 6 MB | boots to usable SSH; tcc OOMs **gracefully** (process SIGSEGV, kernel survives) — 6 MB lacks room for tcc's ~1.5–2 MB working set |

Release (64 MB) is unregressed: Memory + Threading boot tests pass, heap-reclaim
self-test passes. **Trade-off:** a lazy spawn does a 128 KB-contiguous PMM alloc that can
fail under fragmentation (mitigated by the warm floor for the common single-session/
single-process path + the heap→PMM reclaim retry); `verify_stack_memory`'s boot-time
"full pool fits" guarantee weakens to "fits if spawned now," and spawns return `Err`
gracefully on failure. The lazy path is size-profile-only, so it's validated by live
boots rather than the boot self-test suite (which is excluded on `size`).

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
| `no-tests` activates `akuma-net/small-sockets` (MAX_SOCKETS 256→32) | 948 KB |
| `--no-default-features` drops the `neko` editor (gate `dep:akuma-editor`) | **940 KB** |

**What each layer does:**

- **`[profile.size]`** — `opt-level = "z"`, `lto = true`, `codegen-units = 1`, `strip = "symbols"`, `panic = "immediate-abort"`. The last flag converts every panic site into a single `udf` instruction, eliminating the panic formatting infrastructure.
- **`no-tests` feature** — gates all `*_tests` modules and their test-only exported symbols out of the binary entirely (not just skipped at runtime). Also activates `akuma-net/small-sockets`.
- **`neko` feature** (default-on; `dep:akuma-editor`) — the in-kernel `neko` text editor. `scripts/build_size.sh` passes `--no-default-features` so the size profile drops it: `akuma-editor` is no longer linked, removing **8,140 bytes of `.text`** (971,160 → 962,968 bytes total; `.data`/`.bss` unchanged, since neko allocates its line buffers on the heap only while running). Release builds keep neko via the default feature.
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
| `MMAP_FILE_BACKED_LAZY` | false (`release`), true (`size`) | Demand-page file-backed `mmap` regions instead of eagerly allocating all frames. Enabled by default on `size` to avoid PMM exhaustion when ld-musl maps shared libraries. |

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

Verified post-`text_offset` change (June 2026):

| Profile | Binary | `KERNEL_PHYS_BASE` | `IMAGE_RESERVE` | heap_start (observed) | boot floor | free pages |
|---------|--------|-------------------|-----------------|-----------------------|-----------|-----------|
| `extreme` | 801 KB | `0x40100000` | 832 KB (dynamic) | `0x401e3000` @ 4 MB | **4 MB** | 664 |
| `size` | 881 KB | `0x40100000` | 892 KB (dynamic) | `0x40400000` @ 6–8 MB | **6 MB** | 571 (6M) / 1019 (8M) |
| `release` | 2875 KB | `0x40100000` | 2884 KB (dynamic) | `0x405cd000` @ 16–32 MB | **16 MB** | 1837 (16M) / 5933 (32M) |

`heap_start` = `RAM_BASE + code_and_stack` as reported in the boot layout banner.
`IMAGE_RESERVE` auto-tracks the binary via `linker.ld` (`STACK_BOTTOM - KERNEL_PHYS_BASE`);
no per-profile constant to maintain. All three profiles also pass `cargo check` and the
47 ext2 + 4 ELF-boundary host unit tests.

## HTTPS git clone at 4 MB — the scratch path (planned)

**Goal:** clone a git repository over HTTPS onto the machine within the 4 MB boot
floor, without loading the full packfile into RAM.

**Why it fits.** The git smart HTTP protocol is two sequential HTTP requests:

1. `GET /info/refs?service=git-upload-pack` — tiny response (ref list)
2. `POST /git-upload-pack` — client sends want/have negotiation; server streams a packfile

Both are amenable to the file-backed streaming pattern already established by meow's
`post_from_fd` (streaming the request body from an fd) and the `FileWriter` pattern in
the kernel curl command (streaming the response body directly to disk in 8 KB chunks).
The want/have negotiation payload is small (a few KB of text); only the packfile
response is large, and it can be written incrementally to ext2 without ever being
fully resident in RAM.

**RAM budget at 4 MB extreme:**

| Component | Cost |
|---|---|
| embedded-tls record buffers (2 × 16 KB) | ~32 KB |
| 8 KB streaming read chunk | 8 KB |
| Stack + process overhead | < 512 KB |
| **Total** | **< 1 MB** |

Leaves > 1.8 MB free at the 4 MB floor — well above what the kernel needs to stay
responsive.

**The pieces already in place:**

- `userspace/libakuma-tls` — userspace TLS (embedded-tls) over plain TCP sockets;
  `post_from_fd` streams a POST body from an open fd without buffering
- `userspace/libakuma` — `read_fd`, `TcpStream`, ext2-backed file I/O via syscalls
- Kernel ext2 VFS — append writes, so the packfile can land incrementally on disk
- meow's `FileWriter` pattern — 8 KB streaming response → disk, no in-memory
  accumulation

**What scratch needs to add:**

1. `GET /info/refs` → parse the ref list from the streaming response (line-at-a-time,
   no full buffer needed)
2. Build the want/have pkt-line payload in memory (small; a few refs × ~50 bytes)
3. Write it to a temp file; `POST /git-upload-pack` via `post_from_fd` → stream the
   packfile response directly to disk
4. On-disk pack index resolution and object extraction (ext2 as the working buffer)

**Why the kernel's TLS stack is not used here.** The kernel has its own `embedded-tls`
in `crates/akuma-net` for in-kernel HTTPS (the shell `curl` command), but scratch is a
userspace binary. Userspace TLS lives in `libakuma-tls` and runs in the process, not
the kernel. There is no kTLS-style offload (no `SOL_TLS` setsockopt), so sendfile/
splice would not bypass TLS encryption — the streaming read loop is the right approach.

**Relation to the kernel TLS duplication.** The kernel's built-in HTTPS (and its
~58 KB of `embedded-tls` symbols) can be gated off without affecting SSH (which uses a
separate `akuma-ssh-crypto` stack) or scratch (which is userspace). Doing so recovers
~58 KB from the kernel image and eliminates the duplicate TLS stack, at the cost of
requiring a userspace tool for any HTTPS the kernel shell needs.

### Landed: the `kernel-tls` feature (extreme profile drops in-kernel HTTPS)

**Status: landed.** The gating is a dedicated Cargo feature, `kernel-tls`, rather than
the originally-sketched reuse of `tls-rsa` (which only controlled the RSA verifier, not
the whole TLS stack). The split:

- **`akuma-net/kernel-tls`** — gates the `tls`, `tls_rng`, and `tls_verifier` modules
  and makes the TLS-only dependencies (`embedded-tls`, `x509-cert`, `der`, `const-oid`,
  `p256`, `ed25519-dalek`, `sha2`, `rand_core`) `optional`. With the feature off,
  `http::http_get` returns `"HTTPS not supported: kernel TLS is disabled in this build"`
  for any `https://` URL; the plain-HTTP path (and `http_get_streaming`, which was always
  HTTP-only) is unaffected.
- **Top-level `kernel-tls`** — enables `akuma-net/kernel-tls` and pins the matching
  top-level `optional` TLS deps. The shell `curl` command (`src/shell/commands/net.rs`)
  guards `https://` URLs with `cfg!(feature = "kernel-tls")`, printing a clear "use a
  userspace HTTPS tool" message instead of a generic handshake failure; its description
  drops "HTTPS" when the feature is off.
- **`tls-rsa` now implies `kernel-tls`** — RSA cert verification is meaningless without
  the TLS client, so enabling `tls-rsa` pulls `kernel-tls` in automatically.

Profile matrix:

| Profile | `kernel-tls` | In-kernel `curl https://` |
|---------|--------------|---------------------------|
| `release` / default | on (default feature) | yes (ECDSA/Ed25519 + RSA) |
| `size` | on (`build_size.sh` re-adds it) | yes (ECDSA/Ed25519; no RSA) |
| `extreme` | **off** (`build_extreme_size.sh` does not re-add it) | **no** — userspace tool only |

SSH is Ed25519-only via `akuma-ssh-crypto` and shares none of these dependencies, so it
keeps working on every profile.
