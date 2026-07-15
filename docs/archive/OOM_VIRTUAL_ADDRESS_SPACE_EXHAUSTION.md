# Memory Failure Investigation: Virtual Address Space Exhaustion

## Symptom
The `meow` application (and potentially others) crashes with the following kernel log:
```
[mmap] REJECT: size 0x1000 exceeds limit
[exception] Process 21 (/bin/meow) exited, calling return_to_kernel(-1)
```
In some cases, the process itself catches the failure and prints an `OUT OF MEMORY!` error with a high allocation count (e.g., ~196,000).

## Root Cause
The failure is caused by **Virtual Address Space (VAS) exhaustion**, not a physical memory shortage.

### 1. Kernel `mmap` Bump Allocator
The kernel's `mmap` implementation (in `src/process.rs`) uses a simple bump allocator for assigning virtual addresses to new regions.
- It starts at `0x1000_0000`.
- It grows upwards towards the stack (`~0x3FEE_0000`).
- **Critical Flaw**: When a process calls `munmap`, the kernel correctly frees the physical memory, but it **never reclaims the virtual address range**. The `next_mmap` pointer only moves forward.

### 2. Userspace "Page-per-Allocation" Strategy
The `libakuma` allocator (`USE_MMAP_ALLOCATOR = true`) performs a `mmap` syscall for **every single allocation** made by the program.
- Every `String`, `Vec`, or `Box` allocation triggers a `mmap`.
- Every allocation is rounded up to the nearest `PAGE_SIZE` (4 KB).

### 3. The Finite Allocation Budget
The total virtual address space available for `mmap` is approximately 766 MB (`0x2FEE_0000` bytes).
Divided by the 4 KB minimum allocation size, this gives every process a lifetime limit of **196,320 allocations**.
Once this limit is reached, the process can no longer allocate memory, even if it has freed all previous allocations.

### 4. TUI Impact
Applications like `meow` that use a high-frequency TUI render loop (e.g., 20Hz) are particularly vulnerable. Even small strings created via `format!()` in the render loop consume one "slot" of the 196,320 budget. At 100 allocations per second, the budget is exhausted in just **32 minutes**.

## Mitigation Strategy

### Short Term (Workaround)
- **Reduce Allocation Churn**: Minimize `format!()`, `clone()`, and `String::new()` in high-frequency loops. (Partially implemented in recent `meow` refactor).
- **Manual Buffer Reuse**: Use pre-allocated buffers and `core::fmt::write!` instead of creating new strings.

### Medium Term (Kernel Improvement)
- **Reclaim Virtual Addresses**: Update the kernel's `mmap` manager to track free holes in the virtual address space (e.g., using a linked list of free ranges or a more sophisticated VMA tree). This would allow `munmap` to return ranges to the "pool".

### Long Term (Proper Memory Management)
- **Implement a Real Heap Allocator in Userspace**: Instead of the "page-per-allocation" strategy, `libakuma` should use a proper heap allocator (like `talc`, `dlmalloc`, or a simple buddy allocator).
    - The allocator would request large "chunks" of memory from the kernel via `mmap` (e.g., 1 MB at a time).
    - It would then manage small allocations (8 bytes, 64 bytes, etc.) within those chunks.
    - This would reduce the number of syscalls by orders of magnitude and eliminate the virtual address exhaustion for small, short-lived objects.
