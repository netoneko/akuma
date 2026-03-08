# Akuma on Firecracker: Deployment Plan

Goal: boot the current Akuma kernel on a bare-metal AArch64 host using Firecracker.
This is a pre-requisite for the full container demo described in `DEMO_PROPOSAL.md`.

## Instance State (as of 2026-03-08)

- Host: `akuma.sh` — Ubuntu 24.04 aarch64, 2 vCPU (t4g-class), kernel 6.14.0
- Akuma source: `~/akuma/` on `deploy-to-graviton` branch
- Firecracker: **v1.14.2 installed** (`/usr/local/bin/firecracker`)
- TAP device: `tap0` configured at `172.16.0.1/24`
- `/dev/kvm`: **not available** — t4g is Nitro-virtualized; KVM not exposed

## Host Requirement: KVM

Firecracker is a KVM-based VMM. `/dev/kvm` must exist on the host.

**AWS:** Only `metal` instance types expose KVM. None are in the free tier.
The `t4g.micro` at `akuma.sh` cannot run Firecracker.

**Recommended free alternative: Oracle Cloud Always Free — Ampere A1**
- Shape: `VM.Standard.A1.Flex` (1–4 OCPUs, up to 24 GB RAM, always free)
- Architecture: AArch64 (same ISA as Graviton — same binary, same config)
- KVM: exposed; `/dev/kvm` is present after instance launch
- Sign up: cloud.oracle.com

---

## Gap Analysis

| # | Gap | Status | Effort |
|---|-----|--------|--------|
| 1 | RAM base: Akuma links at `0x40000000`, Firecracker DRAM starts at `0x80000000` | **Done** — `firecracker` feature flag + `linker-firecracker.ld` | — |
| 2 | DTB address: fixed scan of `0x4ff00000`; Firecracker passes FDT in `x0` | **Done** — `find_dtb` gated; `x0` used directly | — |
| 3 | Image format: Firecracker needs a flat binary, not ELF | **Done** — `scripts/build-firecracker.sh` runs `objcopy -O binary` | — |
| 4 | GICv3: Firecracker aarch64 uses GICv3; Akuma has a hardcoded GICv2 driver | **Blocking** | High (4–8 h) |
| 5 | Semihosting on exit: already has WFI fallback | Done | — |

---

## Phase 1: Kernel Changes

### ✅ 1-A. DTB Discovery — DONE

`find_dtb()` and `DTB_FIXED_ADDR` are now gated behind
`#[cfg(not(feature = "firecracker"))]`. Under Firecracker, `dtb_ptr` (x0)
is used directly — Firecracker always passes the FDT address in x0 per the
ARM64 boot protocol.

See `docs/FIRECRACKER_FEATURE_FLAG.md` for full details.

---

### ✅ 1-B. Firecracker Linker Script + Feature Flag — DONE

`linker-firecracker.ld` sets `KERNEL_PHYS_BASE = 0x80000000`.
`BOOT_STACK_TOP`, `DEFAULT_RAM_BASE`, `KERNEL_BASE`, `STACK_BOTTOM` are all
cfg-gated in `src/boot.rs` and `src/main.rs`.

Build:
```bash
scripts/build-firecracker.sh
# → akuma-firecracker.bin (2.1 MB flat binary)
```

---

### ✅ 1-C. Flat Binary Format — DONE

`scripts/build-firecracker.sh` runs `objcopy -O binary` after the build.
Firecracker accepts a flat binary at the DRAM base address.

Note: a full 64-byte ARM64 Image header is not strictly required by Firecracker
(it loads the binary at the DRAM base and jumps to offset 0). The plain flat
binary works.

---

### 1-D. GICv3 Driver (4–8 hours — hardest piece)

Firecracker aarch64 emulates a GICv3. Akuma has a hardcoded GICv2 driver (`src/gic.rs`).

**GICv3 differences from GICv2:**

| Feature | GICv2 | GICv3 |
|---------|-------|-------|
| GICD (distributor) | MMIO 0x08000000 | MMIO 0x08000000 (same) |
| CPU interface | GICC MMIO 0x08010000 | ICC_* system registers |
| Per-CPU interface | none | GICR MMIO per CPU (0x080A0000+) |
| IAR read | `ldr from GICC_IAR` | `mrs from ICC_IAR1_EL1` |
| EOI write | `str to GICC_EOIR` | `msr ICC_EOIR1_EL1` |
| Priority mask | `GICC_PMR` | `ICC_PMR_EL1` |

**Firecracker GICv3 MMIO layout (aarch64):**
```
GICD: 0x08000000  size 0x10000
GICR: 0x080A0000  size 0x20000 per CPU (CPU0=0x080A0000, CPU1=0x080C0000, ...)
```

**Minimum GICv3 init sequence:**

```rust
// 1. GICD_CTLR: disable, then enable Group 1 Non-Secure + ARE_NS
write_mmio(GICD_BASE + GICD_CTLR, 0);
dsb();
write_mmio(GICD_BASE + GICD_CTLR, (1 << 4) | (1 << 1)); // ARE_NS | EnableGrp1NS
isb();

// 2. Wake redistributor for this CPU (clear ProcessorSleep bit)
let gicr = GICR_BASE + cpu_id * GICR_STRIDE;
let waker = read_mmio(gicr + GICR_WAKER);
write_mmio(gicr + GICR_WAKER, waker & !(1 << 1)); // clear ProcessorSleep
// Wait until ChildrenAsleep == 0
while read_mmio(gicr + GICR_WAKER) & (1 << 2) != 0 {}

// 3. CPU interface via system registers
write_sysreg!(ICC_PMR_EL1, 0xFF);      // all priorities
write_sysreg!(ICC_BPR1_EL1, 0);        // no preemption
write_sysreg!(ICC_IGRPEN1_EL1, 1);     // enable Group 1 interrupts
isb();
```

**Interrupt acknowledge/EOI (replaces GICC_IAR/GICC_EOIR reads):**
```rust
fn ack_irq() -> u32 {
    let iar: u64;
    unsafe { core::arch::asm!("mrs {}, ICC_IAR1_EL1", out(reg) iar) };
    (iar & 0xFFFFFF) as u32
}

fn eoi_irq(irq: u32) {
    unsafe { core::arch::asm!("msr ICC_EOIR1_EL1, {}", in(reg) irq as u64) };
    isb();
}
```

**Strategy:** add a `gic_version: u8` global set during init (detect from FDT or hardcode per build target), and branch on it in the IRQ fast path.

**Test:** `qemu-system-aarch64 -machine virt,gic-version=3 ...` — VirtIO, timer, SSH all work.

---

## Phase 2: QEMU Pre-Validation

Run these tests on the development machine before touching the AWS instance.
Each removes one QEMU-ism that Firecracker doesn't provide.

```
Test 1: Remove -semihosting              → kernel still boots, WFI fallback works
Test 2: Remove -device loader DTB        → x0 used for DTB, devices detected correctly
Test 3: Boot flat binary (-kernel .bin)  → objcopy output boots identically to ELF
Test 4: gic-version=3                    → GICv3 driver works, timers + VirtIO + SSH work
Test 5: All combined, full suite         → busybox, quickjs, SSH all pass
```

---

## Phase 3: Install Firecracker on the Instance

> **Host note:** Run this on a host with `/dev/kvm`. On AWS, this means a
> `metal` instance. The free alternative is Oracle Cloud `VM.Standard.A1.Flex`.
> The `akuma.sh` t4g instance does not have `/dev/kvm` and cannot run Firecracker.

```bash
# Install latest Firecracker release for aarch64
ARCH=aarch64
VERSION=$(curl -s https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest \
  | grep tag_name | cut -d '"' -f4)
curl -Lo firecracker.tgz \
  "https://github.com/firecracker-microvm/firecracker/releases/download/${VERSION}/firecracker-${VERSION}-${ARCH}.tgz"
tar xzf firecracker.tgz
sudo mv release-${VERSION}-${ARCH}/firecracker-${VERSION}-${ARCH} /usr/local/bin/firecracker
sudo chmod +x /usr/local/bin/firecracker

# Verify
firecracker --version
ls /dev/kvm   # must exist
```

Firecracker v1.14.2 was installed on `akuma.sh` but cannot be used there
(no `/dev/kvm`). The binary and process are validated — only the host needs
to change.

---

## Phase 4: Set Up Networking on the Instance

> TAP device was created on `akuma.sh` (172.16.0.1/24). Repeat on the new host.

Firecracker uses TAP devices for networking. On the host:

```bash
# Create TAP device
sudo ip tuntap add tap0 mode tap
sudo ip addr add 172.16.0.1/24 dev tap0
sudo ip link set tap0 up

# Enable IP forwarding and NAT for the microVM to reach the internet
sudo sysctl -w net.ipv4.ip_forward=1
# Replace eth0 with the instance's primary interface (check: ip route)
PRIMARY_IF=$(ip route | grep default | awk '{print $5}')
sudo iptables -t nat -A POSTROUTING -o $PRIMARY_IF -j MASQUERADE
sudo iptables -A FORWARD -i tap0 -j ACCEPT
sudo iptables -A FORWARD -o tap0 -j ACCEPT
```

The Akuma guest gets `172.16.0.2`. The kernel's smoltcp stack will need this configured (either via FDT, DHCP, or hard-coded during this phase — start hardcoded, refine later).

---

## Phase 5: Create Firecracker Config and Boot

### 5-A. Copy kernel and disk to instance

```bash
# Build flat binary locally
scripts/build-firecracker.sh
# → akuma-firecracker.bin

# Create disk on the instance (faster than copying)
ssh ubuntu@<host> "cd ~/akuma && bash scripts/create_disk.sh 256"

# Copy only the kernel binary
scp akuma-firecracker.bin ubuntu@<host>:~/
```

### 5-B. Create Firecracker config

Save as `~/akuma-fc.json` (field is `show_log_origin`, not `show_origin` —
v1.14.2 rejects unknown fields):

```json
{
  "boot-source": {
    "kernel_image_path": "/home/ubuntu/akuma-firecracker.bin",
    "boot_args": ""
  },
  "drives": [
    {
      "drive_id": "rootfs",
      "path_on_host": "/home/ubuntu/disk.img",
      "is_root_device": true,
      "is_read_only": false
    }
  ],
  "machine-config": {
    "vcpu_count": 1,
    "mem_size_mib": 128
  },
  "network-interfaces": [
    {
      "iface_id": "eth0",
      "guest_mac": "AA:FC:00:00:00:01",
      "host_dev_name": "tap0"
    }
  ],
  "logger": {
    "log_path": "/tmp/fc.log",
    "level": "Debug",
    "show_log_origin": true
  }
}
```

### 5-C. Boot

```bash
# Terminal 1: watch Firecracker logs
tail -f /tmp/fc.log &

# Boot the microVM
sudo firecracker --api-sock /tmp/fc.sock --config-file ~/akuma-fc.json
```

Serial output appears directly in the terminal (Firecracker maps serial to stdout).

### 5-D. Verify

Expected serial output on successful boot:
```
DTB ptr from boot (x0 arg): 0x...   ← non-zero (Firecracker FDT)
[Memory] Found memory at 0x80000000, size 128 MB
[GIC] GICv3 initialized
[VirtIO] Found block device at 0x0a000000
[VirtIO] Found net device at 0x0a001000
[Akuma] SSH ready on port 2222
```

Then from a second terminal on the host:
```bash
ssh -p 2222 akuma@172.16.0.2
```

---

## Phase 6: Expose SSH Port to the Internet

The instance already serves QEMU on ports 2222/8080 via iptables/security groups.
After Firecracker boots, re-point those forwarding rules to `172.16.0.2` instead of `127.0.0.1`.

```bash
# Remove old QEMU forwarding (if any)
sudo iptables -t nat -D PREROUTING -p tcp --dport 2222 -j DNAT --to 127.0.0.1:2222 2>/dev/null || true

# Add Firecracker forwarding
sudo iptables -t nat -A PREROUTING -p tcp --dport 2222 -j DNAT --to 172.16.0.2:2222
sudo iptables -t nat -A PREROUTING -p tcp --dport 8080 -j DNAT --to 172.16.0.2:8080
```

Then: `ssh -p 2222 akuma@akuma.sh` reaches Akuma running in Firecracker on Graviton.

---

## Execution Order

```
Done: 1-A (DTB), 1-B (linker/feature flag), 1-C (flat binary), Phase 3 setup

Next: 1-D (GICv3 driver)                  → only remaining kernel blocker
Next: Phase 2 Tests 4–5                    → QEMU validation with gic-version=3
Next: get a host with /dev/kvm             → Oracle Cloud A1, or AWS metal
Next: Phase 4–6 (TAP, config, boot)        → ship it
```

---

## Open Questions

1. **GICv3 GICR base address** — dump the FDT on a Firecracker instance once
   a KVM host is available: `fdtdump /proc/device-tree` or read from the DTB
   passed in x0. Expected GICR base: `0x080A0000`.
2. **VirtIO legacy vs modern** — Firecracker supports VirtIO MMIO v2 (modern).
   Current QEMU build uses `force-legacy=true`. Firecracker may reject the
   legacy negotiation — verify during Phase 2 QEMU test with `gic-version=3`
   and without the legacy flag.
3. **Network IP config** — smoltcp is configured statically in the kernel.
   For first boot, hardcode `172.16.0.2/24` gateway `172.16.0.1` in
   `src/config.rs` or equivalent; add DHCP later.
4. **Firecracker v1.14.2 JSON field names** — `show_origin` was renamed to
   `show_log_origin`. Already fixed in `akuma-fc.json` above.
