# Dynamic DTB: Removing the Static virt.dtb Loader

## Summary

The kernel previously loaded a pre-built `virt.dtb` file at a hardcoded physical address
(`0x4ff00000`) using QEMU's `-device loader` mechanism. This has been replaced by using
QEMU's natively generated Device Tree Blob, which reflects the actual machine
configuration (including dynamic memory size set via `-m`).

## Motivation

The static DTB approach had several drawbacks:

- **Drift risk:** The static `virt.dtb` file was generated for a fixed configuration
  (256 MB). Changing QEMU's `-m` flag had no effect on detected memory.
- **Extra artifact:** A 1 MB `virt.dtb` file had to be maintained in the project root.
- **Non-standard:** Using `-device loader` to force a DTB at a fixed address bypasses
  QEMU's native DTB generation.
- **Firecracker divergence:** The Firecracker code path already used `x0` correctly.

## ARM64 Linux Image Header

QEMU only passes the DTB address in `x0` for kernels it recognizes as Linux. For ELF
binaries, QEMU reads 64 bytes from the entry point address in guest memory and checks
for the `ARM\x64` magic at offset 0x38. If found, it treats the kernel as Linux and
sets `x0 = DTB address`.

The kernel now includes a 64-byte ARM64 Image header at the `_boot` entry point
(`src/boot.rs`). The first instruction (`code0`) is a branch that skips over the
header to the actual boot code at `_boot_code`.

```
Offset  Size  Field         Value
0x00    4     code0         b _boot_code (branch past header)
0x04    4     code1         0
0x08    8     text_offset   0
0x10    8     image_size    0 (unspecified)
0x18    8     flags         0 (LE, unspecified page size)
0x20    8     res2          0
0x28    8     res3          0
0x30    8     res4          0
0x38    4     magic         0x644d5241 ("ARM\x64")
0x3C    4     res5          0
```

This adds zero runtime cost (one branch instruction) and makes the kernel compatible
with any bootloader that follows the ARM64 boot protocol.

## DTB Discovery Fallback

If `x0` is zero despite the Image header (e.g., older QEMU versions), the kernel falls
back to `scan_for_dtb()`. This scans 2 MB-aligned addresses after the kernel binary
within the boot page table's identity-mapped region (0x40000000-0x7FFFFFFF) for the
FDT magic (`0xd00dfeed`). The scan validates both the magic and the `totalsize` field.

If neither x0 nor the scan finds a DTB, the kernel uses conservative defaults
(256 MB at 0x40000000).

## What Changed

### QEMU launch configurations

Removed `-device loader,file=virt.dtb,addr=0x4ff00000,force-raw=on` from all launch
configs. The runner in `.cargo/config.toml` is now `scripts/cargo_runner.sh`, which
supports a `MEMORY` env var:

```bash
MEMORY=1G cargo run --release    # 1 GB RAM
MEMORY=512M scripts/run.sh       # 512 MB RAM
cargo run --release               # default: 256 MB
```

### Boot assembly (`src/boot.rs`)

- Added 64-byte ARM64 Image header at `_boot`.
- Actual boot code moved to `_boot_code` label (offset +64).

### Kernel DTB discovery (`src/main.rs`)

- Removed `DTB_FIXED_ADDR` constant and `find_dtb()` function.
- Added `scan_for_dtb()` as fallback when x0 is zero.
- `detect_memory()` tries x0 first, then scan, then defaults.

### Deleted files

- `virt.dtb` (1 MB static DTB file in project root).

## Boot Flow

```
QEMU generates DTB matching actual machine config (-m, devices, etc.)
  → writes DTB into guest RAM
  → detects ARM64 Image magic at entry point
  → sets x0 = DTB physical address
  → jumps to _boot
    → _boot: b _boot_code (skip 64-byte header)
    → _boot_code: saves x0, sets up page tables, enables MMU
      → rust_start(dtb_ptr) → kernel_main(dtb_ptr)
        → detect_memory(dtb_ptr) parses FDT /memory node
        → RAM base and size used for heap/PMM init
```
