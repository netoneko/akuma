# libakuma Allocator Options

`libakuma` provides a flexible allocation system to balance memory density and performance.

## Available Features

### 1. Default (Page-per-Allocation)
By default, `libakuma` maps every allocation to a discrete kernel `mmap` region.

*   **Behavior**: `malloc(16)` -> `mmap(4096)`.
*   **Physical Memory**: Memory is returned to the kernel immediately upon `free`.
*   **When to use**: Short-running CLI tools or memory-constrained environments where you want to minimize the process's resident set size (RSS).

### 2. Chunked Allocator (`chunked-allocator`)
Enabled via the `chunked-allocator` Cargo feature.

*   **Behavior**: Requests 64 KB chunks from the kernel and uses the **Talc** allocator to manage small objects within them.
*   **Performance**: Significantly faster (orders of magnitude fewer syscalls).
*   **VAS Protection**: Prevents virtual address space exhaustion for apps that create many small, short-lived strings or objects.
*   **When to use**: TUI apps (`meow`), background services (`herd`), or any app that performs frequent allocations in a loop.

## Usage in `Cargo.toml`

To enable the chunked allocator for your app:

```toml
[dependencies]
libakuma = { path = "../libakuma", features = ["chunked-allocator"] }
```

## Debugging
You can use the following functions to monitor allocator health:
- `libakuma::memory_usage()`: Returns net bytes used by your objects.
- `libakuma::total_allocated()`: Returns total bytes requested from the kernel.
- `libakuma::allocation_count()`: Returns number of logical allocations made.
