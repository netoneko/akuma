# Memory Layout

This document describes the kernel memory layout and important sizing constraints.

## Physical Memory Map

The kernel runs on QEMU's `virt` machine with the following physical address layout:

| Address Range | Size | Description |
|---------------|------|-------------|
| 0x00000000 - 0x3FFFFFFF | 1GB | Device memory (GIC, UART, etc.) |
| 0x40000000 - onwards | Configurable | RAM (default 128MB) |

## Kernel Memory Regions

Within RAM (starting at 0x40000000), memory is divided as follows:

| Region | Size Formula | Description |
|--------|--------------|-------------|
| Code + Stack | max(1/8 of RAM, 32MB) | Kernel binary and stack |
| Heap | 1/2 of RAM | Kernel heap managed by allocator |
| User Pages | Remaining | Physical pages for user processes, managed by PMM |

### Minimum Guarantees

- **Code + Stack**: Always at least 32MB, regardless of total RAM
- This ensures the kernel (up to ~24MB) always has space to load
- The 32MB minimum is enforced in `src/main.rs`

### Default Configuration (128MB RAM)

With the default QEMU settings (`-m 128M`):

```
=== Memory Layout ===
Total RAM: 128 MB at 0x40000000
Code+Stack: 32 MB (0x40000000 - 0x42000000) [min 32MB]
Heap:       64 MB (0x42000000 - 0x46000000) [1/2 of RAM]
User pages: 32 MB (0x46000000 - 0x48000000) [remaining]
=====================
```

### With 256MB RAM

```
=== Memory Layout ===
Total RAM: 256 MB at 0x40000000
Code+Stack: 32 MB (0x40000000 - 0x42000000) [min 32MB]
Heap:       128 MB (0x42000000 - 0x4A000000) [1/2 of RAM]
User pages: 96 MB (0x4A000000 - 0x50000000) [remaining]
=====================
```

## Kernel Binary Size Limit

**Important**: The kernel binary must fit within the Code + Stack region, with room for the stack.

| RAM Size | Code+Stack Region | Max Kernel Size (recommended) |
|----------|-------------------|-------------------------------|
| 128MB | 32MB (minimum) | ~24MB |
| 256MB | 32MB (minimum) | ~24MB |
| 512MB | 64MB (1/8 of RAM) | ~56MB |

The 32MB minimum ensures kernels up to ~24MB always work, even with minimal RAM.

## Boot Logging

The kernel logs all memory layout decisions during boot:

```
[Memory] No DTB pointer, using defaults

=== Memory Layout ===
Total RAM: 128 MB at 0x40000000
Code+Stack: 32 MB (0x40000000 - 0x42000000) [min 32MB]
Heap:       64 MB (0x42000000 - 0x46000000) [1/2 of RAM]
User pages: 32 MB (0x46000000 - 0x48000000) [remaining]
=====================

Heap initialized: 64 MB
```

This helps diagnose memory-related issues by showing exactly how memory was partitioned.

## Known Issue: Binary Size and Loading

### Symptom

When the kernel binary grows too large, the system crashes at boot with:
```
[Exception] Sync from EL1: EC=0x0, ISS=0x0, ELR=0x400xxxxx
```

Debugging with GDB/LLDB shows `udf #0x0` (undefined instruction = zeros) at code addresses that should contain valid instructions.

### Root Cause

The kernel code isn't being fully loaded into memory. This can happen when:
1. The binary size exceeds the Code+Stack region
2. Build artifacts become corrupted during incremental compilation
3. The ELF file has incorrect segment offsets

### Solutions

1. **Clean rebuild**: `cargo clean && cargo build --release`

2. **Increase RAM** (in `.cargo/config.toml`):
   ```
   -m 256M   # or higher
   ```

3. **Check binary size**: Ensure it fits within the Code+Stack region
   ```bash
   rust-size target/aarch64-unknown-none/release/akuma
   ```

4. **Debug with GDB**: Start QEMU with `-s -S` and connect with LLDB:
   ```bash
   lldb target/aarch64-unknown-none/release/akuma -o "gdb-remote 1234"
   ```
   Check if memory at crash address contains actual code or zeros.

## Page Table Configuration

The kernel uses AArch64 4-level page tables with 4KB granule:

- **TTBR0_EL1**: Used for both kernel and user space (identity mapping)
- **TTBR1_EL1**: Points to same tables as TTBR0 (unused, reserved for future)

### Boot page tables

Boot code sets up identity mapping using 1GB blocks:
- L1[0]: Device memory (0x00000000 - 0x3FFFFFFF)
- L1[1]: RAM block 1 (0x40000000 - 0x7FFFFFFF)
- L1[2]: RAM block 2 (0x80000000 - 0xBFFFFFFF)

### User page tables

Each process gets its own TTBR0 page tables. These include:
- **L1[0] → L2 table**: User code/data pages via L3 tables, plus VirtIO device
  pages at L2[80] (0x0a00_0000). GIC, UART, and fw_cfg are NOT mapped here —
  the kernel accesses them via a temporary TTBR0 swap to boot page tables
  (see `docs/DEVICE_MMIO_VA_CONFLICT.md`).
- **L1[1] → L2 table**: Kernel RAM at 0x40000000-0x4FFFFFFF (128 × 2MB blocks),
  plus user mmap region at 0x50000000-0x7FFFFFFF.

## Configuration Files

- **QEMU settings**: `.cargo/config.toml` (RAM size, CPU)
- **Memory layout**: `src/main.rs` (kernel_main function)
- **Linker script**: `linker.ld` (section addresses)
- **Page tables**: `src/boot.rs` and `src/mmu.rs`

## Important: Keeping RAM Size in Sync

The kernel has a fallback `DEFAULT_RAM_SIZE` in `src/main.rs` that's used when DTB
detection fails. **If you change QEMU's `-m` setting, also update `DEFAULT_RAM_SIZE`**:

```rust
// In src/main.rs detect_memory()
const DEFAULT_RAM_SIZE: usize = 128 * 1024 * 1024; // Must match QEMU -m setting
```

DTB detection may fail in some QEMU configurations, so keeping these in sync ensures
correct memory allocation.
