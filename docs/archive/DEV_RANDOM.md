# /dev/urandom and /dev/random

## Problem

bun (and many Linux binaries) require `/dev/urandom` as a file descriptor for
cryptographic randomization. When bun cannot open `/dev/urandom`, it
intentionally crashes by writing to the sentinel address `0xBBADBEEF`:

```
openat(/dev/urandom) ENOENT flags=0x20000
[Fault] Data abort from EL0 at FAR=0xbbadbeef, ELR=0x54668f8, ISS=0x46
```

The kernel already had a `getrandom` syscall (nr=278), but many libraries
open `/dev/urandom` as a file and read from it directly — the syscall alone
is not sufficient.

## Implementation

`/dev/urandom` and `/dev/random` are implemented as virtual device file
descriptors, following the same pattern as `/dev/null`.

### File descriptor type

`FileDescriptor::DevUrandom` in `src/process.rs` — a new variant alongside
`DevNull`. No backing file or path; the kernel generates random data on read.

### openat

`sys_openat` intercepts paths `/dev/urandom` and `/dev/random` before the
VFS lookup. Returns a `DevUrandom` fd. Honors `O_CLOEXEC`.

### read / pread64

Reads from a `DevUrandom` fd call `fill_random_bytes(ptr, len)`, which
fills the user buffer with random data in 256-byte chunks using the kernel
RNG (`crate::rng::fill_bytes`). Returns the requested byte count (reads
from `/dev/urandom` never fail or return short on Linux).

### write / writev

Writes to `/dev/urandom` are silently discarded (return count), matching
Linux behavior. On Linux, writes to `/dev/random` feed the entropy pool;
we don't have an entropy pool, so writes are no-ops.

### fstat

Returns a character device stat with:
- `st_mode = 0o20666` (character device, rw for all)
- `st_rdev = makedev(1, 9)` (major 1, minor 9 — matches Linux)
- `st_ino = 9`

### close

No special cleanup needed. The fd is removed from the process table like
any other.

## Random number source

The kernel RNG is in `src/rng.rs`. It uses AArch64 hardware randomness
when available, seeded from timer jitter. The `fill_bytes` function fills
a buffer with random data and is shared between:

- `getrandom` syscall (nr=278)
- `/dev/urandom` reads
- `/dev/random` reads

All three paths use the same underlying RNG. There is no distinction between
`/dev/random` and `/dev/urandom` — both return cryptographic random bytes
without blocking, matching modern Linux behavior (since kernel 5.6, both
devices draw from the same CSPRNG).

## Also added: /proc/self/exe

bun calls `readlinkat(AT_FDCWD, "/proc/self/exe", ...)` early during startup
to find its own executable path. `sys_readlinkat` intercepts the path
`/proc/self/exe` and returns the current process's name field (the path the
binary was loaded from, e.g., `/bin/bun`). This avoids needing a full procfs
implementation of `/proc/<pid>/exe` symlinks.
