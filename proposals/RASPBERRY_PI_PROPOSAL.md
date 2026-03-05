# Akuma on Raspberry Pi: From Embedded Appliance to Desktop OS

Proposal for running Akuma on Raspberry Pi 4 hardware as both a bare-bones embedded kernel and a full Linux-like operating system, enabled by the kernel modularization plan's generic trait boundaries.

## Why Raspberry Pi

Akuma targets AArch64. The Raspberry Pi 4 runs a Cortex-A72 (AArch64) with a GIC-400 interrupt controller (GICv2) — the same interrupt controller Akuma already supports on QEMU virt. The UART is the same PL011 peripheral, just at a different MMIO address. The timer is the standard ARM generic timer.

This means the kernel core (MMU, scheduler, exception handling, interrupt controller, UART, timer) works without modification. Only two hardware-specific drivers are needed: an SD card block device and an Ethernet NIC.

The composable kernel design makes this a driver swap, not a port.

## Hardware Comparison

| Component | QEMU virt (current) | Raspberry Pi 4 (BCM2711) |
|-----------|--------------------|--------------------------| 
| CPU | Emulated Cortex-A57+ | Cortex-A72, 1.5 GHz |
| ISA | AArch64 | AArch64 |
| RAM | Configurable (256 MB default) | 1, 2, 4, or 8 GB |
| Interrupt controller | GICv2 | GIC-400 (GICv2) |
| UART | PL011 at 0x0900_0000 | PL011 at 0xFE20_1000 |
| Timer | ARM generic timer | ARM generic timer |
| Block device | VirtIO-blk | EMMC2 (SD card) |
| Network | VirtIO-net | BCM54213PE Gigabit Ethernet |
| Boot | `-kernel` flag (QEMU loads ELF) | GPU firmware loads Image from SD |
| Device discovery | FDT (provided by QEMU) | FDT (provided by GPU firmware) |

## Two Target Profiles

### Profile 1: Embedded Appliance (bare-bones)

```
cargo build --release --no-default-features
```

A single-purpose networked device. No SSH, no shell, no TLS. Boots from SD card, mounts ext2, runs one program.

```
Pi 4 powers on
  → GPU firmware loads akuma Image from /boot on SD card
  → Akuma parses FDT from x0, discovers EMMC + UART
  → Mounts ext2 partition on SD card
  → Runs /init (your application)
  → Serial console for debug output
```

**Memory footprint:** ~12-16 MB kernel, leaving 1+ GB for the application.

**Use cases:**
- Network appliance (HTTP API server, MQTT gateway)
- Data logger (read sensors via GPIO, write to SD card)
- Kiosk controller (single application, minimal attack surface)
- IoT gateway (smoltcp TCP/IP + custom protocol handler)
- Teaching / OS development (bare-metal Rust on real hardware)

### Profile 2: Desktop-Class OS (full features)

```
cargo build --release  # default features: ssh, shell, editor, tls, networking
```

A full interactive OS. SSH in, run commands, deploy containers.

```
Pi 4 powers on
  → GPU firmware loads akuma Image from SD card
  → Akuma boots with full feature set
  → SSH server on port 22 (Ed25519 auth)
  → httpd on port 8080 (static files + CGI)
  → dash shell, busybox, quickjs available
  → box container manager for process isolation
  → meow AI assistant (connects to Ollama on network)
```

**Memory footprint:** ~80 MB kernel, leaving 1-8 GB for user processes.

**Use cases:**
- Self-hosting (SSH-accessible development environment)
- Container host (run isolated services via box)
- Home server (HTTP, SSH, custom services)
- Educational platform (explore a real OS from boot to shell)

## Generic Kernel Composition

The modularization plan's generic type parameters make both profiles compile from the same source:

```rust
// Profile 1: Embedded appliance — no networking
type PiAppliance = Kernel<BcmEmmc, Ext2Fs<BcmEmmc>, NoNet, NoSsh>;

// Profile 2: Full OS — all features
type PiFullOs = Kernel<BcmEmmc, Ext2Fs<BcmEmmc>, SmoltcpNet<BcmGenet>, Ed25519Ssh<HwRng>>;

// For comparison — current QEMU target
type QemuKernel = Kernel<VirtioBlock, Ext2Fs<VirtioBlock>, SmoltcpNet<VirtioNet>, Ed25519Ssh<HwRng>>;
```

The ext2 filesystem, VFS, process model, scheduler, ELF loader, and syscalls are identical across all three. Only the block device and network driver swap out.

## What Needs Building

### 1. EMMC2 / SD Card Driver (~500-800 lines)

The BCM2711's EMMC2 controller is an Arasan SD Host Controller (SDHCI-compatible). It supports SD, SDHC, and SDXC cards. The driver needs to:

- Initialize the EMMC2 controller via MMIO registers at 0xFE340000
- Negotiate card voltage and bus width
- Issue CMD17 (read single block) and CMD24 (write single block)
- Implement the `BlockDevice` trait from `akuma-ext2`

```rust
pub struct BcmEmmc {
    base: usize,  // 0xFE340000
}

impl BlockDevice for BcmEmmc {
    fn read_sectors(&self, start: u64, buf: &mut [u8]) -> Result<(), BlockError> { ... }
    fn write_sectors(&self, start: u64, buf: &[u8]) -> Result<(), BlockError> { ... }
    fn sector_size(&self) -> usize { 512 }
}
```

**Reference implementations:**
- [circle](https://github.com/rsta2/circle) (C++ bare-metal Pi library) has a well-documented EMMC driver
- [rust-raspberrypi-OS-tutorials](https://github.com/rust-embedded/rust-raspberrypi-OS-tutorials) has MMIO patterns
- The SD Host Controller Simplified Specification defines the register interface

### 2. BCM54213PE Ethernet Driver (~800-1200 lines)

The Pi 4's Gigabit Ethernet uses the BCM54213PE PHY connected via a GENET (Gigabit Ethernet Network Engine Technology) MAC. The driver needs to:

- Initialize the GENET MAC at 0xFD580000
- Configure the PHY via MDIO
- Set up DMA descriptor rings for TX and RX
- Implement a `NetworkDevice` trait that smoltcp can consume

```rust
pub struct BcmGenet {
    base: usize,  // 0xFD580000
}

impl smoltcp::phy::Device for BcmGenet {
    fn receive(&mut self, ...) -> Option<RxToken> { ... }
    fn transmit(&mut self, ...) -> Option<TxToken> { ... }
    fn capabilities(&self) -> DeviceCapabilities { ... }
}
```

**Reference implementations:**
- Linux `drivers/net/ethernet/broadcom/genet/` (~5000 lines, but includes SMP, power management, etc.)
- The minimal driver (init, TX, RX, no power management) is much smaller

### 3. ARM64 Boot Stub (~100 lines)

The Pi 4's GPU firmware loads the kernel image from the FAT32 `/boot` partition on the SD card. It expects an ARM64 Image format binary. The firmware:

1. Loads the kernel to the address specified in the Image header
2. Sets up a device tree blob (DTB) in memory
3. Passes the DTB address in register `x0`
4. Jumps to the kernel entry point

Akuma's current boot code (`src/boot.rs`) already reads `x0` but may have QEMU-specific assumptions about the memory map. The fix is small:

- Ensure `x0` (FDT address) is read, not hardcoded
- Handle the Pi's memory base (0x00000000, not 0x40000000 like QEMU virt)
- The kernel linker script may need a different base address, or use position-independent code

The `config.txt` on the SD card's `/boot` partition configures the boot:

```ini
arm_64bit=1
kernel=akuma.img
disable_overscan=1
```

### 4. FDT-Driven Device Addresses (already in modularization plan)

The pre-Firecracker validation (Phase 0a, Test 2) already requires removing hardcoded FDT addresses and reading from `x0`. This same fix enables the Pi — the Pi's GPU firmware provides a full FDT with UART, GIC, EMMC, and Ethernet node addresses.

## Pi Model Compatibility

| Model | SoC | GIC | Extra work beyond Pi 4 |
|-------|-----|-----|----------------------|
| **Pi 4 Model B** | BCM2711 | GIC-400 (v2) | None — primary target |
| **Pi 400** | BCM2711 | GIC-400 (v2) | None — same SoC as Pi 4 |
| **Pi 5** | BCM2712 | GIC-500 (v3) | GICv3 driver (~200 lines) |
| **Pi CM4** | BCM2711 | GIC-400 (v2) | None — same SoC, different form factor |
| Pi 3 Model B/B+ | BCM2837 | No GIC | BCM local interrupt controller driver (~200 lines) |
| Pi Zero 2 W | RP3A0 | No GIC | Same as Pi 3 + different Ethernet (USB-attached) |

**Recommended first target:** Pi 4 Model B — GICv2 already supported, most widely available, well-documented BCM2711.

## SD Card Layout

```
SD Card (8+ GB)
├── Partition 1: FAT32 (/boot)
│   ├── config.txt          ← GPU firmware config
│   ├── start4.elf          ← GPU firmware
│   ├── fixup4.dat          ← GPU firmware fixup
│   ├── bcm2711-rpi-4-b.dtb ← device tree (from Pi firmware)
│   └── akuma.img           ← Akuma kernel (ARM64 Image format)
│
└── Partition 2: ext2 (/)
    ├── bin/                 ← busybox, dash, box, herd, qjs, httpd, ...
    ├── etc/                 ← sshd config, herd services
    ├── public/              ← httpd static files, CGI scripts
    ├── proc/                ← (mounted at boot by kernel)
    └── tmp/
```

The FAT32 boot partition is read by the GPU firmware (not Akuma). Akuma only needs to read the ext2 root partition — so no FAT32 driver is required.

## Build and Flash Workflow

```bash
# Build the kernel
cargo build --release --target aarch64-unknown-none

# Convert to ARM64 Image format
aarch64-linux-gnu-objcopy -O binary \
    target/aarch64-unknown-none/release/akuma \
    akuma.img

# Prepare SD card (one-time)
# Partition 1: FAT32 with Pi firmware + config.txt + akuma.img
# Partition 2: ext2 with userspace binaries (same as scripts/populate_disk.sh)

# Update kernel only (subsequent builds)
cp akuma.img /Volumes/boot/akuma.img
```

For development, keep the SD card in a USB reader and reflash the kernel without disconnecting. Or use TFTP netboot (the Pi 4 supports network boot from the GPU firmware) to avoid reflashing entirely.

## Testing Strategy

### Phase 1 — Validate on QEMU (before touching hardware)

All pre-Firecracker tests from DEMO_PROPOSAL.md apply:
- Boot without semihosting
- Boot with QEMU-generated FDT (read from x0)
- Boot as ARM64 Image format binary
- Run acceptance tests (busybox, elftest, quickjs)

### Phase 2 — Serial-only boot on Pi 4

Bring up the kernel with serial output only. No SD card driver yet — load the kernel via TFTP or U-Boot.

**Pass:** Akuma prints to serial console, MMU initialized, timer ticks, GIC handles interrupts.

### Phase 3 — SD card read

Add the EMMC2 driver. Mount the ext2 root filesystem.

**Pass:** `busybox ls /` succeeds.

### Phase 4 — Full boot

Add Ethernet driver. Enable networking features.

**Pass:** SSH into the Pi, run the full acceptance test suite.

## Estimated Effort

| Component | Lines | Depends on |
|-----------|-------|------------|
| EMMC2 SD card driver | ~500-800 | Modularization Phase 3 (BlockDevice trait) |
| BCM54213PE Ethernet driver | ~800-1200 | Modularization Phase 6 (NetworkDevice trait) |
| Boot stub / linker adjustments | ~100 | Modularization Phase 0 (FDT from x0) |
| Build/flash scripts | ~50 | None |
| **Total for minimal (no net)** | **~650-950** | |
| **Total for full** | **~1450-2150** | |

## Cost

- Raspberry Pi 4 Model B (2GB): ~$45
- 32 GB SD card: ~$8
- USB-to-TTL serial adapter: ~$5
- Total: under $60 for a development setup

No recurring cloud costs. The Pi sits on your desk, boots in under a second, and runs until you unplug it.

## Success Criteria

1. Akuma boots on a Raspberry Pi 4 from SD card and prints to serial console
2. The ext2 root filesystem on the SD card is mounted and readable
3. `busybox ls /`, `elftest`, and `quickjs /public/cgi-bin/akuma.js` all pass
4. (Full profile) SSH into the Pi from another machine on the LAN
5. (Full profile) `httpd` serves pages on port 8080 accessible from the network
6. (Stretch) Same kernel binary boots on both QEMU and Pi 4 with FDT-driven device selection
