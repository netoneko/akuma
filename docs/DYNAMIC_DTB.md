# Dynamic DTB: ARM64 Image Header Boot Protocol

## Summary

The kernel uses QEMU's natively generated Device Tree Blob (DTB), which reflects the
actual machine configuration (including dynamic memory size set via `MEMORY` env var).
QEMU passes the DTB address in register `x0` when it recognizes the ARM64 Linux Image
header in the kernel binary.

## ARM64 Linux Image Header

QEMU only passes the DTB address in `x0` for kernels it recognizes as Linux. When
loading a flat binary via `-kernel`, QEMU checks for the `ARM\x64` magic at offset
0x38. If found AND `image_size != 0`, QEMU:

1. Uses `text_offset` to determine the load address
2. Passes the DTB address in `x0`

The kernel includes a 64-byte ARM64 Image header at the `_boot` entry point
(`src/boot.rs`):

```
Offset  Size  Field         Value
0x00    4     code0         b _boot_code (branch past header)
0x04    4     code1         0
0x08    8     text_offset   0 (QEMU adds 2MB, loads at 0x40200000)
0x10    8     image_size    0x300000 (3MB, non-zero to enable text_offset)
0x18    8     flags         0 (LE, unspecified page size)
0x20    8     res2          0
0x28    8     res3          0
0x30    8     res4          0
0x38    4     magic         0x644d5241 ("ARM\x64")
0x3C    4     res5          0
```

### Load Address Details

When QEMU sees the ARM64 magic and `image_size != 0`:
- If `text_offset < 4KB`, QEMU adds 2MB to avoid bootloader overlap
- Our `text_offset = 0` results in kernel loaded at `RAM_BASE + 2MB = 0x40200000`
- DTB is placed at `RAM_BASE = 0x40000000` (first 2MB of RAM)

This layout cleanly separates DTB from kernel code.

## Memory Layout

```
0x40000000  ┌─────────────────┐
            │   DTB (2MB)     │  ← QEMU places DTB here
0x40200000  ├─────────────────┤
            │   Kernel        │  ← _boot entry point
            │   (~2.2 MB)     │
0x40430000  ├─────────────────┤
            │   Boot Stack    │
0x40A00000  ├─────────────────┤
            │   Heap + PMM    │
            └─────────────────┘
```

## Build and Run

The kernel is built as an ELF and converted to a flat binary by `scripts/cargo_runner.sh`:

```bash
rust-objcopy -O binary "$ELF" "$BIN"
qemu-system-aarch64 ... -kernel "$BIN"
```

Set RAM size via the `MEMORY` environment variable:

```bash
MEMORY=1024M cargo run --release   # 1 GB RAM
MEMORY=512M scripts/run.sh         # 512 MB RAM
cargo run --release                # default: 256 MB
```

## DTB Discovery Fallback

If `x0` is zero (shouldn't happen with flat binary + ARM64 header), the kernel checks
`0x40000000` for the FDT magic (`0xd00dfeed`). If not found, it falls back to
conservative defaults (256 MB at 0x40000000).

## Boot Flow

```
cargo run --release
  → cargo_runner.sh converts ELF to flat binary
  → QEMU loads binary at 0x40200000 (sees ARM64 magic, applies 2MB offset)
  → QEMU generates DTB at 0x40000000
  → QEMU sets x0 = 0x48000000 (DTB address for 1GB config)
  → jumps to _boot (0x40200000)
    → _boot: b _boot_code (skip 64-byte header)
    → _boot_code: saves x0, zeros BSS, sets up page tables, enables MMU
      → rust_start(dtb_ptr) → kernel_main(dtb_ptr)
        → detect_memory(dtb_ptr) parses FDT /memory node
        → prints: "[Memory] Detected from DTB: base=0x40000000, size=1024 MB"
```

## Key Files

- `src/boot.rs` — ARM64 Image header and early boot assembly
- `src/main.rs` — `detect_memory()` and `scan_for_dtb()` functions
- `scripts/cargo_runner.sh` — ELF to flat binary conversion
- `scripts/run.sh` — Alternative run script with HVF acceleration
- `linker.ld` — Kernel linked at 0x40200000
