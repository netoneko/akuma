# Memory Layout

This document describes the kernel memory layout and important sizing constraints.

## Physical Memory Map

The kernel runs on QEMU's `virt` machine with the following physical address layout:

| Address Range | Size | Description |
|---------------|------|-------------|
| 0x00000000 - 0x3FFFFFFF | 1GB | Device memory (GIC, UART, etc.) |
| 0x40000000 - 0x401FFFFF | 2MB | DTB (Device Tree Blob, placed by QEMU) |
| 0x40200000 - onwards | Configurable | Kernel + RAM (default 256MB) |

## ARM64 Image Header Boot

The kernel uses the ARM64 Linux Image header format for flat binary loading. This allows
QEMU to pass the DTB address in register `x0`. Key details:

- **Kernel load address**: `0x40200000` (RAM_BASE + 2MB)
- **DTB location**: `0x40000000` (first 2MB of RAM)
- **Boot stack**: At kernel base + 8MB (`0x40A00000`)

The ARM64 Image header (first 64 bytes of the binary) specifies:
- `text_offset = 0` with `image_size != 0` → QEMU adds 2MB, loading at `0x40200000`
- `ARM\x64` magic at offset 56 enables DTB passing in `x0`

## Kernel Memory Regions

Within RAM, memory is divided as follows:

| Region | Size Formula | Description |
|--------|--------------|-------------|
| DTB | 2MB (fixed) | Device Tree Blob at 0x40000000 |
| Code + Stack | max(1/16 of RAM, 8MB) | Kernel binary and boot stack |
| Heap | fixed 16MB | Kernel heap managed by allocator |
| User Pages | Remaining | Physical pages for user processes, managed by PMM |

### Example: 256MB RAM

```
=== Memory Layout ===
Total RAM: 256 MB at 0x40000000
Code+Stack: 16 MB (0x40000000 - 0x41000000) [min 8MB]
Heap:       16 MB (0x41000000 - 0x42000000) [fixed 16MB]
User pages: 224 MB (0x42000000 - 0x50000000) [remaining]
=====================
```

### Example: 1024MB (1GB) RAM

```
=== Memory Layout ===
Total RAM: 1024 MB at 0x40000000
Code+Stack: 64 MB (0x40000000 - 0x44000000) [min 8MB]
Heap:       16 MB (0x44000000 - 0x45000000) [fixed 16MB]
User pages: 944 MB (0x45000000 - 0x80000000) [remaining]
=====================
```

## Kernel Binary Size Limit

**Important**: The kernel binary must fit within the Code + Stack region, with room for the stack.

| RAM Size | Code+Stack Region | Max Kernel Size (recommended) |
|----------|-------------------|-------------------------------|
| 256MB | 16MB | ~8MB |
| 512MB | 32MB | ~24MB |
| 1024MB | 64MB | ~56MB |

The current kernel is ~2.2MB, well within limits.

## Boot Logging

The kernel logs memory layout decisions during boot:

```
DTB ptr from boot (x0 arg): 0x48000000
x0 at _boot entry: 0x48000000
Akuma Kernel starting...
Kernel binary: 2232 KB (0x40200000 - 0x4042e1c0)
[Memory] Detected from DTB: base=0x40000000, size=1024 MB

=== Memory Layout ===
Total RAM: 1024 MB at 0x40000000
Code+Stack: 64 MB (0x40000000 - 0x44000000) [min 8MB]
Heap:       16 MB (0x44000000 - 0x45000000) [fixed 16MB]
User pages: 944 MB (0x45000000 - 0x80000000) [remaining]
=====================
```

## Known Issue: Binary Size and Loading

### Symptom

When the kernel binary grows too large, the system crashes at boot with:
```
[Exception] Sync from EL1: EC=0x0, ISS=0x0, ELR=0x402xxxxx
```

Debugging with GDB/LLDB shows `udf #0x0` (undefined instruction = zeros) at code addresses that should contain valid instructions.

### Root Cause

The kernel code isn't being fully loaded into memory. This can happen when:
1. The binary size exceeds the Code+Stack region
2. Build artifacts become corrupted during incremental compilation
3. The ELF file has incorrect segment offsets

### Solutions

1. **Clean rebuild**: `cargo clean && cargo build --release`

2. **Increase RAM** (via `MEMORY` env var):
   ```bash
   MEMORY=1024M cargo run --release
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
- L0[0] → L1 table for identity mapping
- L1[0]: Device memory (0x00000000 - 0x3FFFFFFF)
- L1[1]: RAM block 1 (0x40000000 - 0x7FFFFFFF) — includes DTB at 0x40000000 and kernel at 0x40200000
- L1[2]: RAM block 2 (0x80000000 - 0xBFFFFFFF)

Device MMIO is also mapped via L0[1] at high virtual addresses:
- L3[0]: GIC distributor (PA 0x08000000 → VA 0x80_0000_0000)
- L3[1]: GIC CPU (PA 0x08010000 → VA 0x80_0000_1000)
- L3[2]: UART (PA 0x09000000 → VA 0x80_0000_2000)
- L3[3]: fw_cfg (PA 0x09020000 → VA 0x80_0000_3000)

### User page tables

Each process gets its own TTBR0 page tables. These include:
- **L1[0] → L2 table**: User code/data pages via L3 tables, plus VirtIO device
  pages at L2[80] (0x0a00_0000). GIC, UART, and fw_cfg are NOT mapped here —
  the kernel accesses them via a temporary TTBR0 swap to boot page tables
  (see `docs/DEVICE_MMIO_VA_CONFLICT.md`).
- **L1[1] → L2 table**: Kernel RAM at 0x40000000-0x7FFFFFFF,
  plus user mmap region at 0x50000000-0x7FFFFFFF.

## Configuration Files

- **RAM size**: Set via `MEMORY` environment variable (e.g., `MEMORY=1024M cargo run --release`)
- **Memory layout**: `src/main.rs` (kernel_main function)
- **Linker script**: `linker.ld` (kernel load address 0x40200000)
- **Page tables**: `src/boot.rs` and `src/mmu.rs`
- **Boot scripts**: `scripts/cargo_runner.sh` and `scripts/run.sh`

## DTB Detection

The kernel reads RAM size from the Device Tree Blob (DTB) passed by QEMU:

1. QEMU passes DTB address in `x0` register (via ARM64 Image header protocol)
2. Kernel parses DTB to get memory base and size
3. Fallback: 256MB if DTB detection fails

```rust
// In src/main.rs detect_memory()
const DEFAULT_RAM_SIZE: usize = 256 * 1024 * 1024; // Fallback if DTB fails
```

With the ARM64 Image header, DTB detection is reliable and the kernel correctly
detects any RAM size configured via the `MEMORY` environment variable.
