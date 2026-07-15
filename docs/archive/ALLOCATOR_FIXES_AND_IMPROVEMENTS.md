# Allocator Fixes and Improvements (February 2026)

## Overview
This document summarizes the major overhaul of the Akuma memory management system to address Virtual Address Space (VAS) exhaustion and improve allocation efficiency.

## 1. Virtual Address Space (VAS) Exhaustion
The system previously suffered from a hard limit on total lifetime allocations (~196,000) per process. This was caused by:
- **Kernel Bump Allocator**: The kernel assigned virtual addresses for `mmap` using a simple upward-growing pointer that never reclaimed addresses.
- **Userspace Page-per-Allocation**: `libakuma` requested a full 4KB page for every object (Strings, Vecs, etc.), leading to rapid exhaustion of the 766MB virtual budget.

## 2. Kernel Mitigation: VA Reclamation
The kernel (`src/process.rs`) now supports full reclamation of virtual address ranges.
- **Free Regions Tracking**: `ProcessMemory` now maintains a `free_regions` list of holes in the virtual address space.
- **First-Fit Allocation**: `alloc_mmap` searches `free_regions` before expanding the bump pointer.
- **Recycling**: `sys_munmap` automatically returns the unmapped range to the reclamation pool.

## 3. Userspace Mitigation: Hybrid Allocation
`libakuma` now offers two allocation strategies via Cargo features:

### Default (Page-Based)
- **Mechanism**: Calls `mmap` for every allocation (rounded to 4KB).
- **Pros**: Returns physical memory to the kernel immediately on `free`. Minimal internal fragmentation.
- **Cons**: High syscall overhead.
- **Best for**: Small, short-lived system utilities.

### Chunked Allocator (`chunked-allocator` feature)
- **Mechanism**: Uses the **Talc** allocator to manage memory within **64KB chunks** requested from the kernel.
- **Pros**: Extremely low syscall overhead. Fast allocations/deallocations. Solves VAS exhaustion for small objects.
- **Cons**: Higher initial physical memory footprint (64KB minimum).
- **Best for**: Long-running apps (like `meow`) or allocation-heavy workloads.

## 4. Verification
The `allocstress` userspace app was used to verify these fixes by performing **2,000,000 allocations** without failure, pushing well past the original crash point.
