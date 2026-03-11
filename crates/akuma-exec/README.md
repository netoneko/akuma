# akuma-exec

Extracted kernel execution subsystem for Akuma OS. Contains process management, threading, MMU, ELF loading, and container (box) registry logic.

## Module Structure

```
src/
├── lib.rs              # Crate root, module declarations
├── runtime.rs          # ExecRuntime callbacks + ExecConfig + IrqGuard
├── kernel_tests.rs     # Kernel-level tests (run_all_tests())
├── threading/
│   ├── mod.rs          # Thread pool, scheduler, context switch, spawn
│   └── types.rs        # Pure types: Context, StackInfo, ThreadState, constants
├── process/
│   ├── mod.rs          # Process lifecycle, PCB, syscall helpers
│   └── types.rs        # Pure types: ProcessMemory, StdioBuffer, UserContext, FD types
├── mmu/
│   ├── mod.rs          # Page table management, TLB ops, UserAddressSpace
│   ├── types.rs        # Pure types: PageTable, flags, constants
│   └── asid.rs         # ASID allocator (pure bit manipulation)
├── elf/
│   ├── mod.rs          # ELF loader, dynamic linker support
│   └── types.rs        # Pure types: ELF headers, parsing utilities, ElfError
└── box_mod/
    ├── mod.rs          # Box (container) registry
    ├── hierarchy.rs    # Ancestry traversal, descendant enumeration
    └── access.rs       # Access control between boxes
```

## Architecture

Each major subsystem is split into:
- **`types.rs`** — Pure data structures and constants with no architecture or runtime dependencies. Fully host-testable.
- **`mod.rs`** — Implementation that may use AArch64 assembly or kernel runtime callbacks. Guarded with `#[cfg(target_os = "none")]` where needed.

Architecture-specific code (inline assembly, `global_asm!`) is gated behind `#[cfg(target_os = "none")]` with no-op stubs provided for host builds. This allows the crate to compile and run tests on the development machine.

## Testing

### Host Tests (79 tests)

Run on your development machine for fast iteration:

```bash
cargo test -p akuma-exec --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Tests cover pure logic in `types.rs` files, ASID allocator, ELF header parsing, box hierarchy/access control, process memory allocation, and more.

### Kernel Tests

`kernel_tests::run_all_tests()` runs inside the kernel at boot time. It tests functionality that requires the runtime environment (thread pool state, ASID allocation with real spinlocks, etc.).

To invoke from the kernel boot sequence:

```rust
akuma_exec::kernel_tests::run_all_tests();
```

## Integration

The crate is initialized by the kernel via:

```rust
akuma_exec::init(runtime, config);
```

where `runtime` provides kernel callbacks (page allocation, printing, IRQ control) and `config` provides tunable parameters (stack sizes, thread counts, etc.).
