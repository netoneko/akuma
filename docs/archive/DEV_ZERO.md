# Kernel prerequisite — `/dev/zero`

Status: **implemented & verified**. This is the one non-feature-gated kernel
change the rump port needs up front (see
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) §6 Phase 2, "Kernel
prerequisite"). Some libc/rump anonymous-memory and buffer-zeroing paths expect
`/dev/zero` to exist; `/dev/null` already did, `/dev/zero` did not.

## Behavior

`/dev/zero` mirrors `/dev/null` in every respect **except reads**:

| op | `/dev/null` | `/dev/zero` |
|----|-------------|-------------|
| `read(n)` | returns 0 (EOF) | fills buffer with `n` zero bytes, returns `n` |
| `write(n)` | discards, returns `n` | discards, returns `n` |
| `stat` rdev | `makedev(1, 3)` | `makedev(1, 5)` |
| `lseek` | returns 0 | returns 0 |

## Implementation

- `crates/akuma-exec/src/process/types.rs` — `FileDescriptor::DevZero` (beside
  `DevNull`/`DevUrandom`).
- `src/syscall/fs.rs` — mirrored **every** `/dev/null` branch: `openat`,
  `read`/`pread64` (zero-fill + return count), `write`/`pwrite64` (discard),
  `lseek`, `fstat`/`newfstatat`/`statx` (`makedev(1, 5)`),
  `fchmodat`/`fallocate`/`ftruncate`.
- `src/vfs/proc.rs` — `DevZero → "/dev/zero"` fd-name arm (exhaustive match).
- `src/process_tests.rs` — `test_dev_zero` (in `run_all_tests`): opens
  `/dev/zero`, reads `N` bytes into a `0xAA`-prefilled buffer and asserts they
  came back all-zero with the full count, writes `N` bytes and asserts the full
  count, via the real `handle_syscall` path with `BYPASS_VALIDATION`.

## Verification

`cargo build --release` clean; boot prints **`[Test] dev_zero PASSED`**. Compiles
in both rump-on (default) and rump-off (`--no-default-features` + sc-gates)
configurations.
