# Proposal: Kernel Heap Improvements — Crash Prevention, Performance, and RAM Budget

## Problem Statement

The kernel currently uses a fixed 16 MB talc heap for all kernel-side allocations (page tables, PCBs, VFS caches, SSH buffers, thread stacks, networking state). There are three interrelated problems:

1. **Faulty allocations crash the kernel.** When talc returns NULL, the Rust global allocator path panics (or the caller isn't prepared). There is no recovery, no back-pressure, and no per-subsystem isolation — one runaway SSH session can OOM the entire kernel.

2. **Performance.** Every allocation takes a global spinlock with IRQs disabled (`TALC.lock()`). All 32 threads contend on a single lock. For high-churn objects (process structs, page table entries, VFS inodes, network buffers) this is a scalability bottleneck.

3. **RAM budget.** With 256 MB total RAM, the kernel consumes ~40 MB before any user process runs:
   - 8–16 MB code + boot stack region
   - 16 MB kernel heap (fixed)
   - ~5 MB thread stacks allocated *on* the heap (8 system × 256 KB + 24 user × 128 KB)
   - Page tables, PMM bitmap, etc.

   That leaves ~216 MB for userspace. Running something like Gemini inside a container needs every byte we can spare.

## Current State

### Allocator (`src/allocator.rs`)
- **Talc** with `ErrOnOom` — a general-purpose `no_std` allocator.
- Single global `Spinlock<Talc>`. Every alloc/dealloc disables IRQs and takes the lock.
- On OOM: logs diagnostics, returns NULL → Rust's `handle_alloc_error` panics.
- A page-based allocator exists but is disabled (`USE_PAGE_ALLOCATOR: bool = false` — comment says "DOES NOT ACTUALLY WORK").
- Debug features (canary, registry) are compile-time off.

### Thread Stacks (from heap)
Thread stacks are `Vec<u8>` allocated on the kernel heap:
- System threads (1–7): 256 KB each = 1.75 MB
- User threads (8–31): 128 KB each = 3 MB
- Async thread: 512 KB
- **Total: ~5.25 MB of heap consumed by stacks alone** (33% of the 16 MB heap)

### OOM Behavior (documented in `docs/OOM_BEHAVIOR.md`)
- Cascading failure: one OOM causes others (exception handler tries to format/allocate → double OOM).
- No per-process or per-subsystem memory limits.
- No way to wait for memory or retry.

### Memory Layout (256 MB QEMU default)
| Region | Size | Notes |
|--------|------|-------|
| Code + Boot Stack | 16 MB | max(RAM/16, 8 MB) |
| Kernel Heap | 16 MB | Fixed `KERNEL_HEAP_SIZE` |
| User Pages (PMM) | 224 MB | Remaining RAM for user processes |

## Proposed Improvements

### Phase 1: Crash Prevention (OOM Resilience)

**Goal:** The kernel must not panic on allocation failure.

#### 1a. Fallible allocations in critical paths

Replace infallible `Box::new()` / `Vec::push()` in critical kernel code with fallible alternatives:

```rust
// Before (panics on OOM):
let pcb = Box::new(Process::new());

// After (returns error):
let pcb = Box::try_new(Process::new())
    .map_err(|_| SyscallError::ENOMEM)?;
```

Key locations to audit:
- `process::spawn` / ELF loader — return ENOMEM to userspace
- SSH session setup — reject connection gracefully
- VFS inode/dentry allocation — return EIO
- `sys_mmap` / demand paging — return ENOMEM or send SIGSEGV

This requires `#![feature(allocator_api)]` which nightly already provides.

#### 1b. Heap watermark / pressure system

Add a simple memory pressure mechanism:

```rust
// In allocator.rs
const HEAP_LOW_WATERMARK: usize = 2 * 1024 * 1024; // 2 MB free

pub fn is_memory_low() -> bool {
    let stats = stats();
    stats.free < HEAP_LOW_WATERMARK
}
```

When memory is low:
- Refuse new SSH connections (return "server busy")
- Refuse new process spawns (return ENOMEM)
- Log a warning once
- Trigger cache eviction (VFS inode cache, closed SSH session buffers)

#### 1c. Bounded buffers for SSH

SSH `line_buffer` and `history` grow unbounded. Cap them:

```rust
const SSH_LINE_BUFFER_MAX: usize = 4096;
const SSH_HISTORY_MAX_ENTRIES: usize = 100;
```

### Phase 2: Lower Kernel RAM Footprint

**Goal:** Maximize RAM available for userspace.

#### 2a. Move thread stacks out of the heap

Thread stacks are the single largest consumer of kernel heap (~5.25 MB / 33%). They should be allocated directly from PMM pages instead:

```rust
// Instead of Vec<u8> on the heap:
fn alloc_thread_stack(size: usize) -> Option<*mut u8> {
    let pages = (size + 4095) / 4096;
    let frame = pmm::alloc_pages_contiguous(pages)?;
    // Map with guard page below
    Some(mmu::phys_to_virt(frame.addr))
}
```

This frees ~5 MB of heap and removes large contiguous allocations that cause fragmentation.

#### 2b. Shrink the kernel heap

With stacks moved out, the heap only needs to hold metadata (PCBs, page tables, VFS structures, network state). **8 MB should be sufficient**, saving 8 MB for userspace:

```rust
// In main.rs
const KERNEL_HEAP_SIZE: usize = 8 * 1024 * 1024; // down from 16 MB
```

This change should be gated behind confirming steady-state heap usage with stacks removed. The heap growth monitor (already in `allocator.rs`) will tell us peak usage.

#### 2c. Reduce code + stack region

The code region is `max(RAM/16, 8 MB)`. The actual kernel binary is ~2 MB, boot stack is 1 MB. With 256 MB RAM this wastes 13 MB. Consider:

```rust
const MIN_CODE_AND_STACK: usize = 4 * 1024 * 1024; // 4 MB (2 MB binary + 1 MB stack + margin)
```

Or better: compute it from the actual `_kernel_phys_end` symbol + 1 MB stack + 1 MB margin, page-aligned.

#### 2d. Right-size thread stacks

Current stack sizes are generous for safety. With stack canaries already in place, we can profile actual usage and shrink:

| Thread Type | Current | Proposed | Savings |
|-------------|---------|----------|---------|
| System (×7) | 256 KB | 128 KB | 896 KB |
| User (×24) | 128 KB | 64 KB | 1.5 MB |
| Async (×1) | 512 KB | 256 KB | 256 KB |

Total potential savings: ~2.6 MB. This should be validated with canary checking — if canaries are never hit at smaller sizes under load, the reduction is safe.

### Phase 3: Slab Allocator for Performance

**Goal:** Reduce lock contention and fragmentation for frequent fixed-size allocations.

#### 3a. Simple slab allocator

A slab allocator pre-allocates pools of fixed-size objects. No need for a full Linux-style SLAB/SLUB — a minimal implementation fits in ~200 lines:

```rust
pub struct SlabCache<const SIZE: usize> {
    free_list: Spinlock<*mut FreeNode>,
    slab_pages: Spinlock<Vec<*mut u8>>,  // backing pages from PMM
    allocated: AtomicUsize,
}

impl<const SIZE: usize> SlabCache<SIZE> {
    pub fn alloc(&self) -> Option<*mut u8> { ... }
    pub fn free(&self, ptr: *mut u8) { ... }
}
```

Key properties:
- **O(1) alloc/free** — just pop/push from a free list
- **No fragmentation** — all objects are the same size
- **Per-slab locks** — SSH slab doesn't block VFS slab
- **Grows on demand** — allocates new pages from PMM when the free list is empty

#### 3b. Candidate slab caches

| Cache | Object Size | Estimated Count | Benefit |
|-------|------------|-----------------|---------|
| `Process` (PCB) | ~1–2 KB | ≤64 | Fast spawn/exit |
| Page table pages | 4 KB | Many | Avoids heap fragmentation |
| VFS inodes | ~256 B | Dozens | Faster file ops |
| SSH session state | ~4 KB | ≤8 | Bounded, fast cleanup |
| Network buffers | 2 KB | ≤16 | Matches TX_PACKET_BUFFER_SIZE |

#### 3c. Fallback to general heap

Slab caches are optional optimizations. If a slab is full and can't grow (PMM exhausted), fall back to the general talc heap. If that also fails, return the error. This layered approach means slabs improve the common case without adding failure modes.

### Phase 4: Monitoring and Observability

#### 4a. Per-subsystem memory accounting

Tag allocations by subsystem so `free` / `/proc/meminfo` can report:

```
Heap:       8 MB total, 3.2 MB used
  VFS:      800 KB
  Process:  1.1 MB
  SSH:      400 KB
  Network:  200 KB
  Other:    700 KB
Stacks:     5.25 MB (PMM-backed)
User pages: 232 MB total, 45 MB used
```

#### 4b. OOM killer (future)

If memory pressure is critical and cache eviction isn't enough, kill the largest userspace process (like Linux's OOM killer). This is a last resort but prevents total kernel deadlock.

## RAM Budget: Before and After

### Current (256 MB)
| Component | Size |
|-----------|------|
| Code + Boot Stack | 16 MB |
| Kernel Heap | 16 MB |
| Thread stacks (on heap) | ~5 MB |
| PMM bitmap + metadata | ~1 MB |
| **Available for userspace** | **~223 MB** |

### After Phases 1–2 (256 MB)
| Component | Size |
|-----------|------|
| Code + Boot Stack | 4–8 MB |
| Kernel Heap | 8 MB |
| Thread stacks (PMM-backed) | ~3 MB |
| PMM bitmap + metadata | ~1 MB |
| **Available for userspace** | **~236–240 MB** |

**Net gain: ~13–17 MB more for userspace.** With 512 MB RAM (Firecracker), proportionally more is saved from the code region.

### Scaling to 1 GB+ for Gemini-in-a-box

If the goal is running a large model inference binary inside a container:
- Give QEMU/Firecracker 1 GB+ RAM
- Kernel overhead stays at ~15 MB (fixed heap + stacks + code)
- ~1009 MB available for the container
- The container's `box` process gets its own address space with demand-paged memory

## Implementation Priority

| Phase | Effort | Impact | Priority |
|-------|--------|--------|----------|
| 1a: Fallible allocations | Medium | High (crash prevention) | **P0** |
| 1b: Heap watermark | Low | Medium (prevents cascade) | **P0** |
| 1c: Bounded SSH buffers | Low | Medium (leak prevention) | **P1** |
| 2a: Stacks out of heap | Medium | High (frees 5 MB heap) | **P1** |
| 2b: Shrink heap to 8 MB | Low | High (8 MB to userspace) | **P1** (after 2a) |
| 2c: Shrink code region | Low | Medium (4–8 MB savings) | **P2** |
| 2d: Right-size stacks | Low | Low-Medium (2.6 MB) | **P2** |
| 3: Slab allocator | Medium-High | Medium (perf + fragmentation) | **P2** |
| 4: Monitoring | Medium | Low (observability) | **P3** |

## Risks

- **Fallible allocations** require auditing many call sites. Missing one means the old panic behavior persists. Mitigated by grep + incremental rollout.
- **Shrinking the heap** could cause OOM under workloads we haven't tested. Mitigated by doing it after stacks are moved out and validating with peak usage data.
- **Slab allocator bugs** (use-after-free, double-free to slab) are subtle. Mitigated by keeping the implementation simple and adding debug assertions.
- **Smaller thread stacks** risk overflow. Mitigated by canary checking and load testing before committing to smaller sizes.
