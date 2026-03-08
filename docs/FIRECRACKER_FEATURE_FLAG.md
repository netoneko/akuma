# Firecracker Feature Flag

## What Changed

Added a `firecracker` Cargo feature that gates all platform-specific constants
for running Akuma in Firecracker (Graviton / bare-metal AArch64) vs. QEMU virt.

### Files Modified

**`Cargo.toml`**
```toml
[features]
firecracker = []
```

**`src/boot.rs`**

`KERNEL_PHYS_BASE` and `STACK_TOP` are now injected into `global_asm!` via
`const` template operands instead of being hardcoded literals:

```rust
#[cfg(not(feature = "firecracker"))]
const PHYS_BASE: usize = 0x4000_0000;   // QEMU virt
#[cfg(feature = "firecracker")]
const PHYS_BASE: usize = 0x8000_0000;   // Firecracker

#[cfg(not(feature = "firecracker"))]
const BOOT_STACK_TOP: usize = 0x4080_0000;
#[cfg(feature = "firecracker")]
const BOOT_STACK_TOP: usize = 0x8080_0000;

global_asm!(
    r#"
    .equ KERNEL_PHYS_BASE, {phys_base}
    .equ STACK_TOP,        {stack_top}
    ..."#,
    phys_base = const PHYS_BASE,
    stack_top = const BOOT_STACK_TOP,
);
```

The page tables in `setup_boot_page_tables` already map the 0x80000000 range
as normal RAM (L1[2]), so no page table changes are needed.

**`src/main.rs`**

Four constants are cfg-gated:

| Constant | QEMU | Firecracker |
|----------|------|-------------|
| `KERNEL_BASE` | `0x4000_0000` | `0x8000_0000` |
| `STACK_BOTTOM` | `0x4070_0000` | `0x8070_0000` |
| `DEFAULT_RAM_BASE` | `0x4000_0000` | `0x8000_0000` |
| `DTB_FIXED_ADDR` | `0x4ff0_0000` | *(not compiled in)* |

`find_dtb()` (which scans the fixed QEMU DTB load address) is also gated
behind `#[cfg(not(feature = "firecracker"))]`. Under Firecracker, the FDT
address is always in x0 per the ARM64 boot protocol and used directly.

**`linker-firecracker.ld`** — new linker script:
- `KERNEL_PHYS_BASE = 0x80000000`
- `KERNEL_VIRT_BASE = 0xFFFF000080000000`
- `STACK_BOTTOM = 0x80700000`

**`scripts/build-firecracker.sh`** — build + objcopy helper:
```bash
RUSTFLAGS="-C link-arg=-Tlinker-firecracker.ld" \
    cargo build --release --features firecracker
llvm-objcopy -O binary target/.../akuma akuma-firecracker.bin
```

### Build Commands

```bash
# QEMU (unchanged)
cargo build --release

# Firecracker flat binary
scripts/build-firecracker.sh
# → akuma-firecracker.bin (2.1 MB)
```

## Deployment Attempt

Built `akuma-firecracker.bin` locally (cross-compiled via `cargo objcopy`),
copied to the `akuma.sh` Graviton instance, installed Firecracker v1.14.2,
created TAP device `tap0` (172.16.0.1/24), wrote `akuma-fc.json`.

Boot failed immediately:
```
Kvm error: Error creating KVM object: No such file or directory (os error 2)
```

`/dev/kvm` does not exist on the instance. The `t4g.micro` instance type is
Nitro-virtualized — KVM is not exposed to guests. Firecracker requires KVM.

## KVM Requirement

Firecracker is a KVM-based VMM. It will not run on any AWS instance type
that is itself a VM (all `t`, `m`, `c`, `r` families). Only **metal** instance
types expose `/dev/kvm` on AWS, and none are in the free tier.

## Recommended Host: Oracle Cloud Always Free (Ampere A1)

Oracle's Always Free tier includes up to **4 OCPUs + 24 GB RAM** of Ampere A1
(AArch64, same ISA as Graviton). These instances expose `/dev/kvm`.

- Sign up: cloud.oracle.com
- Instance shape: `VM.Standard.A1.Flex` (1–4 OCPUs, 1–24 GB RAM)
- OS: Ubuntu 22.04 or 24.04 aarch64
- Verify: `ls /dev/kvm` → should exist after launch

The same `akuma-firecracker.bin`, `disk.img`, `akuma-fc.json`, and TAP setup
from the deployment plan work unchanged on OCI.

## What Remains

See `proposals/FIRECRACKER_DEPLOYMENT_PLAN.md` — the remaining blockers before
Akuma actually boots in Firecracker are GICv3 support and the ARM64 Image
header. The feature flag and binary format are now in place.
