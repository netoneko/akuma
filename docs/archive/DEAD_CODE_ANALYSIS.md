# Dead Code Analysis

This document summarizes dead code and potential simplifications identified in the codebase.

## 1. brk Syscall (No Longer Needed)

**Location**: `src/syscall.rs`, `src/process.rs`, `userspace/libakuma/src/lib.rs`

**Status**: Dead code when `USE_MMAP_ALLOCATOR = true` (current default)

### What It Is

The `brk` syscall provides traditional Unix heap allocation by moving the "program break" (end of data segment). It's an alternative to `mmap` for dynamic memory.

### Why It's Dead

In `userspace/libakuma/src/lib.rs`:

```rust
pub const USE_MMAP_ALLOCATOR: bool = true;  // mmap is the default
```

With this setting:
- `alloc()` → uses `mmap_alloc()`
- `dealloc()` → uses `mmap_dealloc()`
- `realloc()` → uses `mmap`
- All brk-based allocation functions are **never called**

### What Can Be Removed

**Kernel side** (`src/syscall.rs`):
- `sys_brk()` function
- `nr::BRK` constant

**Kernel side** (`src/process.rs`):
- Global `PROGRAM_BRK` and `INITIAL_BRK` (if still present)
- `init_brk()`, global `get_brk()`, `set_brk()` functions

**Userspace side** (`userspace/libakuma/src/lib.rs`):
- `brk()` function
- `brk_init()`, `brk_expand()`, `brk_alloc()`, `brk_realloc()` in allocator
- Update `print_allocator_info()` to not call `brk(0)`

### Recommendation

Keep brk for now as optional fallback (harmless dead code), or remove entirely for cleaner codebase.

---

## 2. KERNEL_CONTEXTS Array - FIXED

**Status**: ✅ RESOLVED

The `KERNEL_CONTEXTS` array has been removed. Kernel context is now stored per-process in `Process.kernel_ctx` (a `KernelContext` struct containing sp, x19-x28, x29, x30).

The `run_user_until_exit` function is now a naked function that:
1. Saves callee-saved registers to `proc.kernel_ctx` 
2. Sets up user context and ERETs to user mode
3. When process exits, `return_to_kernel()` restores context and `ret`s back to caller

---

## 3. Summary Table

| Item | Location | Status |
|------|----------|--------|
| `sys_brk()` | syscall.rs | Dead (mmap default) |
| `KERNEL_CONTEXTS[]` | process.rs | ✅ Removed |
| `Process.kernel_ctx` | process.rs | ✅ Used correctly |

---

## 4. Cleanup Priority

| Priority | Item | Effort | Impact |
|----------|------|--------|--------|
| Low | Remove brk syscall | 30 min | Cleaner code |

None of these are urgent since the current code works. Clean up when convenient.
