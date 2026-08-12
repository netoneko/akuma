# libakuma Allocator Options

`libakuma` provides a flexible allocation system to balance memory density and performance.

## Available Features

### 1. Chunked Allocator (`chunked-allocator`) — default

Enabled by default (`default = ["chunked-allocator"]` in `libakuma/Cargo.toml`).

*   **Behavior**: Requests 64 KB chunks from the kernel and uses the **Talc** allocator to manage small objects within them.
*   **Performance**: Significantly faster (orders of magnitude fewer syscalls).
*   **VAS Protection**: Prevents virtual address space exhaustion for apps that create many small, short-lived strings or objects.
*   **When to use**: The default for a reason — TUI apps (`meow`), background services (`herd`), and any app doing frequent allocations in a loop all want this.

### 2. Page-per-Allocation (`default-features = false`)

Opting out of `chunked-allocator` maps every allocation to a discrete kernel `mmap` region instead.

*   **Behavior**: `malloc(16)` -> `mmap(4096)`.
*   **Physical Memory**: Memory is returned to the kernel immediately upon `free`.
*   **When to use**: Short-running CLI tools or memory-constrained environments where you want to minimize the process's resident set size (RSS) over allocation throughput.

Both arms are mmap-backed; there is no brk-based allocator anymore (see
`docs/archive/LIBAKUMA_AUDIT.md` item 13 — the old `USE_MMAP_ALLOCATOR` switch
and its racy `brk_alloc` fallback were deleted, since nothing ever set it to
`false`).

## Usage in `Cargo.toml`

Chunked is on by default; to opt out and use page-per-allocation instead:

```toml
[dependencies]
libakuma = { path = "../libakuma", default-features = false }
```

## Debugging
You can use the following functions to monitor allocator health:
- `libakuma::memory_usage()`: Returns net bytes used by your objects.
- `libakuma::total_allocated()`: Returns total bytes requested from the kernel.
- `libakuma::allocation_count()`: Returns number of logical allocations made.
