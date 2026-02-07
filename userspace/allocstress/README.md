# allocstress

`allocstress` is a high-churn memory allocation test designed to stress-test the kernel's virtual address reclamation and the userspace chunked allocator.

## Purpose
It reproduces the "Virtual Address Space Exhaustion" bug by creating and destroying millions of strings. 

- **Success Condition**: Reaching 2,000,000 allocations without a kernel "REJECT" or "OUT OF MEMORY" error.
- **Fail Condition**: Crashing before the target, usually due to virtual address leaks.

## Implementation Details
- Uses `libakuma` with the `chunked-allocator` feature.
- Performs tight loops of `String` creation and dropping.
- Reports progress every 10,000 iterations.

## Running
Once built and copied to the disk image:
```bash
/bin/allocstress
```

Expected output:
```
allocstress: starting virtual address exhaustion test
Allocations: 10000 (actual count: ...)
...
allocstress: reached 2,000,000 allocations without failure!
allocstress: done
```
