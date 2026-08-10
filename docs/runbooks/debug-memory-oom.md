# Debug memory / OOM / panics

Symptom-driven debugging for kernel OOM, heap corruption, and allocation
panics. Memory has historically been the highest-churn subsystem (peak fire
window June 2026; many fixes land in `LOW_MEMORY_ENVIRONMENT.md`).

> **Stability of this area: C (active risk).** High commit density through
> 2026-06; two items still OPEN (per-run heap creep; reclaim below ~5 MB).
> Most failure modes below are FIXED — but new low-RAM workloads keep surfacing
> edge cases. Trust the tables, but verify with the monitors in §"Knobs".

For architecture, see
[`../reference/subsystems/memory.md`](../reference/subsystems/memory.md).

## The failure classes

Memory bugs in Akuma cluster into a few recurring classes. Knowing the class
narrows the cause fast:

| Class | Signature | Why it happens |
|---|---|---|
| **Region-boundary miscalc** | `EC=0x21/0x22` garbage PC, SP inside heap; kernel/heap/stack overlap at low RAM | A computed region (heap vs boot stack, kernel-VA-end vs identity map) used a wrong constant. The dominant root-cause pattern across this whole subsystem. |
| **CoW/refcount desync** | `DOUBLE-FREE=N` (non-zero) on the `[Mem]` line | `user_frames` refcount vs `cow_ref` got out of sync (~30 hand-maintained call sites) |
| **Heap metadata race** | Garbled console (dropped `e`/0x65→0x05); intermittent `FAR=0x5` | Allocator not IRQ-guarded; timer fired mid-alloc |
| **PMM scarcity / fragmentation** | `[OOM] …` then `panic!` / `brk #1` | PMM drained to 0; or fragmentation → no contiguous run |
| **VA-space exhaustion** | `[mmap] REJECT: size … exceeds limit` | mmap bump pointer not reclaimed (now first-fit) |
| **Demand-page fault** | `[Fault] Data abort from EL0 at FAR=… ISS=0x7` → SIGSEGV (-11) | User touched unmapped memory / reserve hit 0 |

## Symptom → cause → fix

| Symptom (signature) | Cause | Status | Fix / workaround |
|---|---|---|---|
| `!!! PANIC !!! memory allocation of N bytes failed` (N≈25 MB) in kernel | `sys_read`/`append_file` slurped whole files into kernel heap | FIXED | `read_at` streaming; 64 KB syscall cap; 16 MB ext2 cap |
| `[OOM] allocation of N bytes failed — killing process` then exit -12 | libakuma brk/mmap returns 0 | FIXED (kernel) | `#[alloc_error_handler]` (`src/allocator.rs:499`) calls `return_to_kernel(-12)`. **NOTE:** `archive/OOM_RECOVERY_OPTIONS.md` says "no handler" — that doc is STALE. |
| `[mmap] REJECT: size 0x1000 exceeds limit` / ~196,000 allocs then OOM | VA-space exhaustion (mmap bump never reclaimed) | FIXED | `ProcessMemory::free_regions` first-fit + chunked-allocator (Talc 64 KB chunks) |
| `realloc` hangs | `munmap` deadlocks when called from inside realloc | FIXED | Deferred-free queue (`DEFERRED_FREE_SLOTS=16`), flushed on next `dealloc` |
| `[Exception] EC=0x25 FAR=0x5` | `read_current_pid()` read PROCESS_INFO @0x1000 through boot TTBR0 (device-mem garbage) | FIXED | TTBR0-range guard in `read_current_pid()` (boot 0x4020_0000–0x4400_0000 → None) |
| Garbled console + intermittent `FAR=0x5` | `talc_alloc` not IRQ-guarded; timer fired mid-alloc → heap metadata race | FIXED | `talc_alloc`/`talc_realloc` wrapped in `with_irqs_disabled`; whole realloc atomic |
| `EC=0x0E` Illegal Execution State | SPSR EL bit set → bad ERET | FIXED | Clear IL bit (bit 20) in SPSR before ERET |
| `EC=0x22 ELR==FAR==<garbage>` (low RAM, e.g. apk @64 MB) | Three bugs: user_frames refcount over-free; ELF heap-slurp; heap/boot-stack overlap | FIXED | `FreeOutcome` + refcount-aware free; size-gated deferred loader (`HEAP_SLURP_MAX`=1 MiB); `code_and_stack` covers `BOOT_STACK_TOP+1 MB` guard |
| `EC=0x3c ISS=0x1` `brk #1` (kernel abort) | Kernel alloc failed when PMM drained to 0; PMM fragmented → no contiguous run | FIXED | `USER_PAGE_RESERVE`=16; `heap_grow_backoff` halves run toward `needed` |
| `Failed to load ELF: Failed to create address space` | `UserAddressSpace::new()` OOM during page-table alloc | FIXED (mitigated) | Admission pressure checks (`is_memory_low`) in fork/clone/spawn; `Box::try_new` → ENOMEM |
| `[Fault] Data abort from EL0 … ISS=0x7` → SIGSEGV (-11) | Demand-paging reserve hit 0 / unmapped access | PARTLY OPEN | Reserve returns None → clean SIGSEGV (process killed, kernel survives). Reclaim below 5 MB incomplete. |
| `EC=0x25 FAR=0xffff…7cc8` thread-spawn (extreme) | `clone_thread` read slot stack before on-demand alloc; `WARM_FREE_USER=0` → stack_top=0 wraps | FIXED | Allocate slot stack immediately after claiming |
| `iss=0xe` permission fault at `0x0800_0000` (bun 93 MB) | User heap collided GIC device page in shared L0[0] | FIXED | Devices remapped to L0[1] @ `0x80_0000_0000+`; only DFSC 0x04/0x08 demand-page |
| One-time ~256 KB heap growth + ~17–50 KB/run creep (meow) | ext2 block cache pinned Talc span; per-process teardown leak | PARTLY FIXED | BlockRingCache (64-entry, single backing) / extreme=no-cache. **Per-run creep: OPEN.** |

## Memory floors (current)

If you're below these, you'll hit OOM by design — raise `MEMORY`.

| Workload | Floor | Profile |
|---|---|---|
| Boot to serving SSH (usable) | **4 MB** | extreme (6 MB on `size`) |
| `meow -c` one-shot LLM | **4.0 MB** | extreme |
| `tcc -static hello.c` | **4 MB** | extreme (dynamic `/usr/bin/tcc` = 6 MB) |
| Full meow agentic pipeline | **4 MB** | extreme |
| `rustc hello.rs` | ≥ 256 MB | release (HVF sweet spot 768 MB–1 GB) |

Below 4 MB QEMU aborts `Not enough space for DTB` (guest-layout limit, not
kernel). Floors move with `MEM_CALC_CLAMP_MB`, `MIN_CODE_AND_STACK_BYTES`,
`STACK_GUARD_BYTES` (`src/config.rs`).

## Knobs & monitors

There is **no `/proc/meminfo`**. Observability is the serial `[Mem]` line +
the `pmm` shell command.

| Knob | Where | What it shows |
|---|---|---|
| `pmm` shell cmd | shell | `Total / Allocated / Free` pages + MB |
| `[Mem]` periodic line | `memory_monitor()`, `src/main.rs`; gated `MEM_MONITOR_ENABLED` | RAM free, heap free/used/peak, allocs, threads, **`DOUBLE-FREE=N`** (non-zero ⇒ desync), spans pinned/live |
| `DOUBLE-FREE` counter | `pmm::double_free_count()` | Non-zero ⇒ CoW/refcount desync — investigate immediately |
| Demand-page counters | `DP_FILE_PAGES/DP_ANON_PAGES/DP_COW_PAGES/DP_PROTNONE_PAGES`, `EAGER_MMAP_PAGES` | Attributes RAM spikes to file/anon/CoW paths |
| `SpanReport` | `allocator::claimed_span_report()` | `pinned` not falling back to 0 after workload exit = the "free PMM never recovered" bug |
| `is_memory_low()` | `src/allocator.rs:556` | Circuit breaker (free heap < 2 MB); checked at fork/clone/spawn/SSH accept |
| PSTATS | per-process, on exit | `mmap/munmap/recvfrom…(Nms)`. **Note:** mmap time is preemption-inflated (IRQs on during syscalls) — not real CPU |
| `DEBUG_FRAME_TRACKING` | `src/pmm.rs:20` (**off**) | `pmm leaks` grouping. Off by default — the BTreeMap tracker corrupted under load. |
| `ENABLE_CANARIES` | `src/allocator.rs:25` (**off**) | Stack/guard canaries. **Breaks virtio-drivers DMA** — targeted debug only |

## Debug procedure

1. Boot with `MEM_MONITOR_ENABLED=true` (in `src/config.rs`); watch `[Mem]` lines.
2. If `DOUBLE-FREE=N` (non-zero): CoW/refcount desync. Grep for recent
   `track_user_frame` / `remove_user_frame` / `cow_ref_*` changes.
3. If heap `pinned` never recovers after a workload exits: span-pinning
   (fragmentation). Check `claimed_span_report()`.
4. For a crash: note the `EC` / `FAR` / `ISS`, match to a row in the table
   above.
5. Under TCG (`HVF=0`) for faithful PC; `GDB=1` + `lldb -p :1234` for live
   inspection.

## Background

- `archive/LOW_MEMORY_ENVIRONMENT.md` — the densest single source (44 commits); extreme-profile hardening.
- `archive/HEAP_AND_MEMORY_IMPROVEMENTS.md` — the watermark + admission-control design.
- `archive/OOM_BEHAVIOR.md`, `archive/OOM_RECOVERY_OPTIONS.md` — partly STALE (handler now exists).
- `archive/POST_EXIT_PMM_RECLAIM.md` — proves there is no single-process leak; the floor symptom is raw PMM scarcity.
