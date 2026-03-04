# Node.js Missing Syscalls

Status of running Node.js (musl, static-PIE, --jitless) on Akuma.

## Test Command

```
node --jitless --version
```

## Issues Found & Fixes

### 1. Unknown syscall 90 — `capget` (FIXED)

Node.js calls `capget(2)` at startup to check Linux capabilities
(e.g., `CAP_NET_BIND_SERVICE`). On AArch64 Linux, this is syscall 90.

**Fix:** Added as a stub that zeroes the data struct (no capabilities),
returns 0. Node.js treats empty capabilities gracefully.

### 2. Kernel heap OOM on file-backed mmap (FIXED)

`sys_mmap` for file-backed mappings allocated a kernel-heap buffer
equal to the **entire mapping length** to read file data, then copied
into user pages. When the dynamic linker loaded shared libraries
(each potentially several MB), the temporary buffers exhausted the
32 MB kernel heap.

**Fix:** File-backed mmap now reads page-by-page directly into the
already-allocated user physical pages via `phys_to_virt`, eliminating
the large intermediate buffer entirely. Peak kernel heap usage per
mmap dropped from O(mapping_size) to O(4KB).

Additionally, the kernel heap was increased from a fixed 32 MB to
`max(RAM/4, 32MB)` to provide headroom for other bookkeeping.

### 3. `rt_sigaction` — stub only

Node.js registers ~30 signal handlers at startup. Currently stubbed
(returns 0, does nothing). This is sufficient for `--version` but
real signal delivery (SIGPIPE, SIGCHLD, etc.) will be needed for
I/O-heavy workloads.

## Syscall Trace (startup sequence)

| Syscall | Number | Status |
|---------|--------|--------|
| capget | 90 | stub (zeroed caps) |
| rt_sigaction | 134 | stub (returns 0) |
| rt_sigprocmask | 135 | stub (returns 0) |

## Memory Requirements

Node.js binary is ~300 MB of virtual address space (code_end ≈ 0x12fce000).
With deferred lazy segments, only accessed pages are physically backed.
Minimum recommended configuration:

- QEMU RAM: 256 MB (current default)
- Kernel heap: 64 MB (1/4 of RAM)
- User pages: ~160 MB

For heavier Node.js workloads (running scripts, npm), 512 MB+ RAM is
recommended.

## Remaining Work

- **Signal delivery**: Implement actual signal dispatch for SIGPIPE,
  SIGCHLD, SIGTERM so child processes and I/O errors work correctly.
- **More syscalls**: Running actual JS scripts will likely need
  additional syscalls (epoll, eventfd, clock_nanosleep, etc. — most
  already implemented).
- **Memory pressure**: Monitor heap usage under load; V8 (even jitless)
  can mmap large regions for its managed heap.
