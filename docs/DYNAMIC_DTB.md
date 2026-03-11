# Dynamic DTB: Removing the Static virt.dtb Loader

## Summary

The kernel previously loaded a pre-built `virt.dtb` file at a hardcoded physical address
(`0x4ff00000`) using QEMU's `-device loader` mechanism. This has been replaced by using
QEMU's natively generated Device Tree Blob, which QEMU passes to the kernel via register
`x0` per the standard ARM64 boot protocol.

## Motivation

The static DTB approach had several drawbacks:

- **Drift risk:** The static `virt.dtb` file could become stale if the QEMU machine
  configuration changed (different memory size, added/removed devices, etc.).
- **Extra artifact:** A 1 MB `virt.dtb` file had to be maintained in the project root,
  along with a `qemu-roms/` directory of symlinks.
- **Non-standard:** The ARM64 boot protocol specifies that the bootloader (QEMU, in our
  case) passes the FDT address in `x0`. Using a `-device loader` to force a DTB at a
  fixed address bypasses this.
- **Firecracker divergence:** The Firecracker code path already used `x0` correctly.
  Having two different DTB discovery paths added unnecessary complexity.

## What Changed

### QEMU launch configurations

Removed `-device loader,file=virt.dtb,addr=0x4ff00000,force-raw=on` from:

- `.cargo/config.toml`
- `scripts/run.sh`
- `scripts/run_with_gdb.sh`
- `scripts/run_on_kvm.sh` (both KVM and TCG paths)
- `scripts/docker/docker-compose.yml`

QEMU's `-kernel` flag on aarch64 virt automatically generates a DTB reflecting the
actual machine configuration and passes its address in `x0`.

### Kernel DTB discovery (`src/main.rs`)

- Removed `DTB_FIXED_ADDR` constant (`0x4ff00000`).
- Removed `find_dtb()` function that scanned the fixed address as a fallback.
- Unified `detect_memory()` to use `dtb_ptr` (from `x0`) directly for both QEMU and
  Firecracker, eliminating the `#[cfg(not(feature = "firecracker"))]` branching around
  DTB pointer resolution.
- If `x0` is zero (should not happen with QEMU `-kernel`), the kernel falls back to
  conservative defaults (256 MB at 0x40000000).

### Deleted files

- `virt.dtb` (1 MB static DTB file in project root).

### Unchanged

- **`src/boot.rs`** — The assembly already saves `x0` into `x19` and passes it to
  `rust_start`. No changes needed.
- **Boot page tables** — QEMU places its generated DTB within the RAM range
  (0x40000000–0x4FFFFFFF for 256 MB), which is identity-mapped by the L1[1] boot page
  table entry. No mapping issues.
- **Device address constants** — GIC, UART, fw_cfg, and VirtIO addresses in
  `crates/akuma-exec/src/mmu/types.rs` remain hardcoded. These match QEMU virt's fixed
  layout regardless of how the DTB is provided. Full device discovery from DTB is a
  separate effort (needed for Raspberry Pi / alternate platforms).

## Boot Flow (After)

```
QEMU generates DTB matching actual machine config
  → places DTB in RAM (typically near end of RAM)
  → jumps to _boot with x0 = DTB physical address
    → _boot saves x0, sets up page tables, enables MMU
      → rust_start(dtb_ptr) → kernel_main(dtb_ptr)
        → detect_memory(dtb_ptr) parses FDT for /memory node
          → RAM base and size used for heap/PMM init
```

## Verification

After booting, the kernel log should show:

```
DTB ptr from boot (x0 arg): 0x4XXXXXXX   (non-zero, QEMU's DTB address)
x0 at _boot entry: 0x4XXXXXXX            (same value)
[Memory] Detected from DTB: base=0x40000000, total=256 MB, usable=... MB
```
