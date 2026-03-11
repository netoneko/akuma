# akuma-exec Refactoring Summary

## Motivation

The `akuma-exec` crate was excluded from `default-members` because AArch64 inline assembly prevented host compilation. This made it impossible to run unit tests during development without booting the full kernel in QEMU. The refactoring separates testable logic from architecture-specific code, enabling fast host-side testing.

## Changes

### 1. Module Splits

Each major monolithic file was split into a directory module with pure types extracted:

| Original File | New Structure | Pure Types Extracted |
|---|---|---|
| `threading.rs` | `threading/mod.rs` + `threading/types.rs` | `Context`, `StackInfo`, `ThreadState`, `ThreadSlot`, stack calculation functions |
| `process.rs` | `process/mod.rs` + `process/types.rs` | `ProcessMemory`, `StdioBuffer`, `UserContext`, `SignalAction`, FD types, `YieldOnce` |
| `mmu.rs` | `mmu/mod.rs` + `mmu/types.rs` + `mmu/asid.rs` | `PageTable`, MMU flags/constants, `AsidAllocator` |
| `elf_loader.rs` | `elf/mod.rs` + `elf/types.rs` | `Elf64Ehdr`/`Elf64Phdr` parsing, `ElfError`, auxv constants |
| `box_registry.rs` | `box_mod/mod.rs` + `box_mod/hierarchy.rs` + `box_mod/access.rs` | Ancestry traversal, access control logic |

### 2. Conditional Compilation

All AArch64 inline assembly is gated with `#[cfg(target_os = "none")]`:
- `runtime.rs` — `IrqGuard` asm for DAIF manipulation
- `threading/mod.rs` — `global_asm!` context switch, `set_current_exception_stack`, `get_current_thread_register`, SGI handlers, extern asm function declarations (with host stubs)
- `mmu/mod.rs` — TLB flush, TTBR read/write, boot TTBR0 lookup
- `process/mod.rs` — `enter_user_mode` (ERET to EL0), TTBR0 read, LR read

The `no_std` attribute is conditional: `#![cfg_attr(not(test), no_std)]` so that `std` is available during host test builds.

### 3. Nested Box Support

Extended the container system for hierarchical boxes:
- Added `parent_box_id: Option<u64>` to `BoxInfo`
- `hierarchy.rs` — `get_ancestry_chain()`, `is_ancestor()`, `get_children()`, `get_descendants()`, `validate_nested_root()`
- `access.rs` — `can_access_box()`, `can_kill_box()`, `cascade_kill_order()`

### 4. Host Tests (79 tests)

Added `#[cfg(test)]` modules to all pure type files:
- `threading/types.rs` — Context validity, StackInfo operations, stack requirement calculations
- `process/types.rs` — ProcessMemory allocation, StdioBuffer I/O, YieldOnce future, signal defaults
- `mmu/types.rs` — Page table init, flag calculations, attr_index
- `mmu/asid.rs` — ASID allocation/free lifecycle, exhaustion
- `elf/types.rs` — ELF header parsing, endian utilities, error formatting
- `box_mod/hierarchy.rs` — Ancestry chains, descendant enumeration, root validation
- `box_mod/access.rs` — Access control rules, cascade kill ordering

### 5. Kernel Tests

`kernel_tests.rs` provides `run_all_tests()` callable from the kernel boot sequence. Tests exercise runtime-dependent functionality: thread pool state, ASID allocation with real locking, current thread ID validity, and stack requirement verification.

### 6. Workspace Integration

`akuma-exec` is added back to `default-members` in the workspace `Cargo.toml`, so `cargo test --target <host>` includes it automatically.

## Testing

```bash
# Host tests (fast, no QEMU needed)
cargo test -p akuma-exec --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)

# Kernel target check
cargo check -p akuma-exec

# Full kernel build
cargo check
```
