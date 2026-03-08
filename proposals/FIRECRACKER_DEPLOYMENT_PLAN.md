# Akuma on Firecracker: Deployment Plan

Goal: boot the current Akuma kernel on a Graviton EC2 instance using Firecracker.
This is a pre-requisite for the full container demo described in `DEMO_PROPOSAL.md`.

## Instance State

- Host: `akuma.sh` — Ubuntu 24.04 aarch64, 2 vCPU, kernel 6.14.0
- Akuma source: `~/akuma/` (full git checkout)
- Akuma ELF binary: `~/akuma.kernel/akuma`
- Firecracker: **not installed**

---

## Gap Analysis

Four things prevent the current kernel from booting in Firecracker:

| # | Gap | Severity | Effort |
|---|-----|----------|--------|
| 1 | RAM base address: Akuma links at `0x40000000`, Firecracker DRAM starts at `0x80000000` | **Critical** | Medium (2–4 h) |
| 2 | GICv3: Firecracker aarch64 uses GICv3; Akuma has a hardcoded GICv2 driver | **Critical** | High (4–8 h) |
| 3 | DTB address: Akuma scans fixed `0x47f00000`; Firecracker passes DTB address in `x0` | **Critical** | Small (1 h) |
| 4 | Image format: Firecracker expects a flat binary with ARM64 Image header, not ELF | **Critical** | Small (1 h) |
| 5 | Semihosting on exit: already has WFI fallback — no change needed | Low | None |

---

## Phase 1: Kernel Changes

### 1-A. Fix DTB Discovery (1 hour)

**File:** `src/main.rs`, `find_dtb()`

Currently `find_dtb()` ignores `dtb_ptr` (x0) and checks a hardcoded address `0x47f00000`.
Firecracker generates its own FDT and passes the address in x0.

**Change:** try x0 first; fall back to the fixed scan only if x0 is 0 or invalid.

```rust
fn find_dtb(dtb_ptr_from_x0: usize, ...) -> usize {
    // Try x0 first (Firecracker / standard ARM64 boot protocol)
    if dtb_ptr_from_x0 != 0 {
        let magic = unsafe { core::ptr::read_volatile(dtb_ptr_from_x0 as *const u32) };
        if magic == 0xedfe0dd0 {
            return dtb_ptr_from_x0;
        }
    }
    // Fall back to fixed QEMU load address
    let magic = unsafe { core::ptr::read_volatile(DTB_FIXED_ADDR as *const u32) };
    if magic == 0xedfe0dd0 { return DTB_FIXED_ADDR; }
    0
}
```

Then pass `dtb_ptr` from `rust_start` into `find_dtb`.

**Test:** remove `-device loader,file=virt.dtb,...` from `scripts/run.sh`, confirm QEMU still boots using QEMU-generated FDT via x0.

---

### 1-B. ARM Base Address: Firecracker Linker Script (2–4 hours)

**Current:** `linker.ld` links kernel at physical `0x40000000`.
**Firecracker:** DRAM starts at `0x80000000`.

Create a second linker script `linker-firecracker.ld` (copy of `linker.ld` with `KERNEL_PHYS_BASE = 0x80000000`).
Also update the default RAM size constants for Firecracker (128 MB at 0x80000000).

Build variant:

```bash
# In .cargo/config.toml add a profile or use RUSTFLAGS override:
RUSTFLAGS="-C link-arg=-Tlinker-firecracker.ld" \
  cargo build --release
```

Or add a Cargo feature `firecracker` that selects the right linker arg.

**Constants to update in `src/main.rs`:**
```rust
#[cfg(feature = "firecracker")]
const DEFAULT_RAM_BASE: usize = 0x80000000;
#[cfg(not(feature = "firecracker"))]
const DEFAULT_RAM_BASE: usize = 0x40000000;
```

**Stack addresses in `linker-firecracker.ld`:**
```
KERNEL_PHYS_BASE = 0x80000000;
STACK_TOP    = 0x80800000;   /* 8 MB from Firecracker base */
STACK_BOTTOM = 0x80700000;
```

---

### 1-C. ARM64 Image Format (1 hour)

Firecracker on aarch64 requires a flat binary with the standard ARM64 Linux Image header (64 bytes).
Ref: https://www.kernel.org/doc/html/latest/arm64/booting.html

Add a 64-byte header at the very start of `.text.boot`:

```asm
/* ARM64 Image header — must be first in the binary */
_image_header:
    b _boot               /* branch to actual boot code (replaces magic) */
    .long 0               /* reserved */
    .quad 0               /* image_load_offset: 0 = load at 2MB-aligned base */
    .quad _image_end - _image_header   /* image_size */
    .quad 0xa             /* flags: LE, 4K pages, phys placement any */
    .quad 0               /* reserved x3 */
    .quad 0
    .quad 0
    .ascii "ARM\x64"      /* magic: 0x644d5241 */
    .long 0               /* reserved (PE header offset, 0 for non-PE) */
```

Then add a build step to `scripts/run.sh` and a new `scripts/firecracker-build.sh`:

```bash
aarch64-linux-gnu-objcopy -O binary \
  target/aarch64-unknown-none/release/akuma \
  akuma-firecracker.bin
```

Firecracker config points at `akuma-firecracker.bin`.

**Test:** `qemu-system-aarch64 -kernel akuma-firecracker.bin ...` — same behavior as the ELF.

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

SSH to `ubuntu@akuma.sh`, then:

```bash
# Install latest Firecracker release for aarch64
ARCH=aarch64
VERSION=$(curl -s https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest \
  | grep tag_name | cut -d '"' -f4)
curl -Lo firecracker.tgz \
  "https://github.com/firecracker-microvm/firecracker/releases/download/${VERSION}/firecracker-${VERSION}-${ARCH}.tgz"
tar xzf firecracker.tgz
sudo mv release-${VERSION}-${ARCH}/firecracker-${VERSION}-${ARCH} /usr/local/bin/firecracker
sudo mv release-${VERSION}-${ARCH}/jailer-${VERSION}-${ARCH} /usr/local/bin/jailer
sudo chmod +x /usr/local/bin/firecracker /usr/local/bin/jailer

# Verify
firecracker --version
```

---

## Phase 4: Set Up Networking on the Instance

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
# From local machine — build Firecracker variant first
RUSTFLAGS="-C link-arg=-Tlinker-firecracker.ld" cargo build --release
aarch64-linux-gnu-objcopy -O binary \
  target/aarch64-unknown-none/release/akuma akuma-fc.bin

scp -i ~/.ssh/netoneko-aws.pem \
  akuma-fc.bin disk.img \
  ubuntu@akuma.sh:~/
```

### 5-B. Create Firecracker config

Save as `~/akuma-fc.json`:

```json
{
  "boot-source": {
    "kernel_image_path": "/home/ubuntu/akuma-fc.bin",
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
    "show_origin": true
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
Week 1: 1-A (DTB), 1-C (Image format)   → easy wins, unblock testing
Week 1: 1-B (linker base address)         → enables building the Firecracker image
Week 1: Phase 2 Tests 1–3                 → validate before touching GIC

Week 2: 1-D (GICv3 driver)               → hardest piece
Week 2: Phase 2 Tests 4–5                 → full QEMU pre-validation

Week 2: Phase 3–6 (install + deploy)      → ship it
```

---

## Open Questions

1. **GICv3 GICR base address in Firecracker** — verify from `firecracker --version` + FDT dump on the instance once Firecracker is installed. Expected: `0x080A0000`.
2. **Network config in guest** — does smoltcp pick up IP from DHCP or is it hardcoded? For the first boot, hardcode `172.16.0.2/24` gateway `172.16.0.1` directly in `src/config.rs`.
3. **Firecracker version** — check which release is current for aarch64; confirm GICv3 is used (it has been since Firecracker 0.24).
4. **VirtIO legacy vs modern** — Firecracker supports VirtIO MMIO (modern, not legacy). Current QEMU uses `-global virtio-mmio.force-legacy=true`. Firecracker may require removing the legacy flag — verify during Phase 2 QEMU testing.
