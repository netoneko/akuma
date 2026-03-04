# Node.js Missing Syscalls

Status of running Node.js (musl, static-PIE, --jitless) on Akuma.

## Test Command

```
node --jitless --version
```

**Status: WORKING** — exits cleanly with code 0.

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

### 3. `sys_fcntl` missing fd validation — 46 MB kernel heap leak (FIXED)

**Symptoms:** After fixes #1 and #2, Node.js still crashed with
`ALLOC FAIL` at 58 MB kernel heap usage (91% of 63 MB). The crash
occurred after ~4.16 million `fcntl` syscalls.

**Root cause:** `sys_fcntl` did not validate that the file descriptor
existed before modifying the `cloexec_fds` BTreeSet. Node.js/musl
startup iterates over a large range of fd numbers calling
`fcntl(fd, F_SETFD, FD_CLOEXEC)`. On Linux, this returns `EBADF` for
non-existent fds, causing the loop to terminate. Our implementation
silently succeeded and inserted every fd number into the BTreeSet,
creating ~700K unique BTreeSet nodes at 56-152 bytes each — totaling
46 MB of kernel heap that was never freed.

**Fix:** Added fd existence check at the top of `sys_fcntl`:
```rust
if proc.get_fd(fd).is_none() {
    return EBADF;
}
```
This matches Linux behavior. The loop now terminates after checking
the small number of actually-open fds (~3-5), reducing fcntl calls
from 4.16M to a handful and eliminating the heap leak entirely.

**Diagnostic journey:** This was a difficult leak to find because:
- The allocation (56 bytes) was too small to stand out individually
- BTreeSet only allocates on node splits (~1 in 11 inserts), so not
  every fcntl call triggered a visible allocation
- Initial SC-LEAK detector threshold of >16 bytes missed the leak
  because each individual fcntl call only leaked 0 or 56 bytes net
- The leak accumulated across 4.16M calls to reach 46 MB
- Lowering the SC-LEAK threshold to >0 with cumulative tracking
  finally revealed: `[SC-LEAK] nr=25 +56 bytes cum=39931KB calls=500000`

### 4. `rt_sigaction` — stub only

Node.js registers ~30 signal handlers at startup. Currently stubbed
(returns 0, does nothing). This is sufficient for `--version` but
real signal delivery (SIGPIPE, SIGCHLD, etc.) will be needed for
I/O-heavy workloads.

## Syscall Profile (successful run)

| Syscall | Number | Count | Status |
|---------|--------|-------|--------|
| capget | 90 | 1 | stub (zeroed caps) |
| rt_sigaction | 134 | ~31 | stub (returns 0) |
| rt_sigprocmask | 135 | 4 | stub (returns 0) |
| fcntl | 25 | ~5 | EBADF for invalid fds |
| mmap | 222 | 151 | working (5139 pages) |
| munmap | 215 | 48 | working |
| mprotect | 226 | 20 | working |
| openat | 56 | 56 | working |
| close | 57 | 19 | working |
| read | 63 | 23 | working |
| write | 64 | ~560 | working |
| brk | 214 | 6 | working |
| clock_gettime | 113 | 3 | working |
| ioctl | 29 | 9 | working |
| fstat/newfstatat | 80 | 21 | working |
| getpid | 172 | 1 | working |

Page faults (demand paging): 28 faults, 6736 pages mapped via readahead.

## Memory Requirements

Node.js binary is ~47 MB on disk, ~300 MB virtual address space
(code_end = 0x12fce000). With deferred lazy segments and demand
paging (256-page readahead), only accessed pages are physically backed.

Successful run profile:
- Kernel heap: 12 MB baseline, ~13 MB peak during run
- Physical pages: ~6736 demand-paged + 5139 mmap'd = ~46 MB
- Total RAM needed: ~60 MB minimum

Recommended configuration:
- QEMU RAM: 256 MB (current default)
- Kernel heap: 64 MB (1/4 of RAM)

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
- **Remove diagnostics**: The SC-LEAK detector, DA-LEAK/IA-LEAK
  detectors, syscall counters, and heap growth monitors added during
  this investigation can be removed or gated behind a debug flag once
  stability is confirmed.
