# Plan: Implement `flock` Syscall

## Context

`flock` (syscall 32 on AArch64) is currently stubbed in `src/syscall/mod.rs` as `nr::FLOCK => 0` (always succeeds). Many userspace programs rely on `flock` for advisory file locking (e.g., package managers, editors, lock-file patterns). This plan implements real advisory locking semantics with kernel tests and documentation.

---

## Design

### Lock semantics
- Advisory locks per **inode** (path-resolved to inode via VFS `resolve_inode()`)
- Holder tracked as `(pid, fd)` pair — deviation from Linux's "per open file description" model (acceptable for Akuma's use cases)
- Operations: `LOCK_SH=1` (shared), `LOCK_EX=2` (exclusive), `LOCK_UN=4` (release), `LOCK_NB=8` (non-blocking modifier)
- Conflict rules: EX blocks all; SH blocks only EX

### Blocking strategy
- Spin-yield loop calling `crate::threading::yield_cpu()` (or equivalent scheduler yield) up to ~1000 iterations
- Each iteration re-checks the lock table
- If exhausted without acquiring: return `EAGAIN` (same as `EWOULDBLOCK`)
- `LOCK_NB`: skip spin loop, return `EWOULDBLOCK` immediately on conflict

### Lock cleanup
- On `sys_close(fd)`: if the closing `KernelFile` held a lock, release it from the global table

---

## Implementation Steps

### 1. New file: `src/syscall/flock.rs`

Define global lock table and `sys_flock`:

```rust
use alloc::collections::BTreeMap;
use spinning_top::Spinlock;

struct FlockEntry {
    shared_holders: BTreeSet<(u32, u32)>,  // (pid, fd) pairs
    exclusive_holder: Option<(u32, u32)>,   // (pid, fd)
}

static FLOCK_TABLE: Spinlock<BTreeMap<u32, FlockEntry>> = Spinlock::new(BTreeMap::new());

pub(super) fn sys_flock(fd: u32, op: u32) -> u64 { ... }
pub(super) fn flock_release_fd(pid: u32, fd: u32) { ... }  // Called from sys_close
```

Key logic in `sys_flock`:
1. Get current process → validate fd → get path from `KernelFile`
2. Resolve inode via `crate::vfs::resolve_inode(&path)` (returns `u32`)
3. Strip `LOCK_NB` flag, determine `lock_type` (SH / EX / UN)
4. For `LOCK_UN`: remove `(pid, fd)` from table, return 0
5. For `LOCK_SH` / `LOCK_EX`: check conflicts → spin-yield loop → insert holder → return 0
6. If `LOCK_NB` and conflict: return `EWOULDBLOCK`

### 2. Modify `src/syscall/mod.rs`

Replace stub:
```rust
// Before:
nr::FLOCK => 0,

// After:
nr::FLOCK => flock::sys_flock(args[0] as u32, args[1] as u32),
```

Add module declaration: `mod flock;`

### 3. Modify `src/syscall/fs.rs` — `sys_close`

After removing the FD from the process table, call:
```rust
flock::flock_release_fd(proc.pid, fd);
```

### 4. Modify `crates/akuma-exec/src/process/types.rs` — `KernelFile`

Add field to track held lock (for cleanup on close):
```rust
pub struct KernelFile {
    pub path: String,
    pub position: usize,
    pub flags: u32,
    pub dir_cache: Option<Vec<DirCacheEntry>>,
    pub flock: Option<FlockType>,  // NEW: SH or EX if this fd holds a lock
}

pub enum FlockType { Shared, Exclusive }
```

This avoids needing to scan the entire lock table on close.

### 5. Add kernel tests in `src/process_tests.rs` (or new `src/flock_tests.rs`)

Tests to write (each returns `bool`):

| Test | Description |
|------|-------------|
| `test_flock_exclusive_basic` | Open file, acquire EX lock, release |
| `test_flock_shared_basic` | Open file, acquire SH lock, release |
| `test_flock_shared_shared_compatible` | Two SH locks on same inode succeed |
| `test_flock_exclusive_blocks_shared` | EX held → SH with LOCK_NB returns EWOULDBLOCK |
| `test_flock_shared_blocks_exclusive` | SH held → EX with LOCK_NB returns EWOULDBLOCK |
| `test_flock_release_on_close` | Close fd → lock released, next open can acquire EX |
| `test_flock_upgrade` | SH → EX upgrade (lock_nb) |
| `test_flock_invalid_op` | Invalid op returns EINVAL |

Register in `run_all_tests()` / at kernel init (same pattern as `src/fs_tests.rs`).

### 6. Add `docs/FLOCK_ADVISORY_LOCKS.md`

Document:
- Overview and motivation
- Lock semantics (inode-based, advisory)
- Supported operations table
- Deviation from Linux semantics (per open-file-description vs per-fd)
- Blocking behavior and spin-yield approach
- Known limitations (no inter-process block notification, spin-based blocking)
- Example usage (shell script pattern)

---

## Critical Files

| File | Change |
|------|--------|
| `src/syscall/mod.rs` | Replace stub, add `mod flock;` |
| `src/syscall/fs.rs` | Call `flock_release_fd` in `sys_close` |
| `src/syscall/flock.rs` | **New** — global table + `sys_flock` + `flock_release_fd` |
| `crates/akuma-exec/src/process/types.rs` | Add `flock: Option<FlockType>` to `KernelFile` |
| `src/process_tests.rs` | New flock tests |
| `docs/FLOCK_ADVISORY_LOCKS.md` | **New** — documentation |

## Reuse

- `crate::vfs::resolve_inode(path)` — get inode for lock key (already in VFS trait)
- `spinning_top::Spinlock` — already used everywhere for the lock table
- `crate::threading::yield_cpu()` — for spin-yield in blocking path (verify exact name)
- `proc.get_fd()`, `proc.pid` — standard process/fd patterns from `akuma-exec`
- Error constants `EWOULDBLOCK`, `EBADF`, `EINVAL`, `ENOSYS` — already in scope

---

## Verification

1. **Compile**: `cargo check` — no errors in modified crates
2. **Host tests**: `cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)` for `akuma-vfs` and `akuma-exec` crates
3. **Kernel tests**: Boot in QEMU (`cargo run --release`) — flock test suite output in console
4. **Integration**: In POSIX shell (dash), run `flock -x /tmp/test.lock echo locked` — should succeed
5. **Edge cases**: Test `LOCK_NB` with conflicting lock in kernel test suite
