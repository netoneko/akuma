# Run Akuma on Firecracker locally

**Stability: C** — active work. The `platform-firecracker` build target exists and
links correctly, but **no Akuma boot under Firecracker has been observed yet**.
Steps marked *(unverified)* have not been executed end-to-end. Steps marked
*(verified)* were run on 2026-08-21.

Firecracker needs `/dev/kvm` and an aarch64 host. On an Apple-silicon Mac that
means a Linux VM with nested virtualization — macOS itself never has `/dev/kvm`,
and **Docker Desktop cannot provide it** (see §5). On AWS it means a `.metal`
instance; this runbook is the local path.

Design background and the constants behind every address here:
`proposals/FIRECRACKER_PORT.md`.

---

## 1. Host prerequisites *(verified)*

Apple silicon **M3 or newer** and **macOS 15+**. Nested virtualization is a
hardware+OS capability; check it rather than assuming:

```bash
cat > /tmp/nv.swift <<'EOF'
import Virtualization
if #available(macOS 15.0, *) {
    print("nested: \(VZGenericPlatformConfiguration.isNestedVirtualizationSupported)")
} else { print("macOS < 15") }
EOF
swift /tmp/nv.swift
```

Expect `nested: true`. On an M1/M2, or macOS < 15, it prints `false` and there is
no local path — go to the AWS metal route in `proposals/FIRECRACKER_PORT.md` §7.

## 2. A Linux guest with `/dev/kvm` *(partially verified)*

```bash
brew install lima
limactl start -y --name=fc --vm-type=vz --nested-virt --cpus=4 --memory=8
```

`--vm-type=vz` is required — `--nested-virt` means nothing under the QEMU driver.
Both settings land in `~/.lima/fc/lima.yaml` as `vmType: vz` and
`nestedVirtualization: true`; confirm with:

```bash
grep -E '^vmType|nestedVirtualization' ~/.lima/fc/lima.yaml
```

*(verified: instance creation and both settings. The first `start` downloads an
~800 MB Ubuntu cloud image and is slow.)*

**`limactl start` buffers all output when stdout is not a TTY.** A backgrounded
run therefore looks dead while it is working, and it is easy to launch several by
accident — they then race on the same instance and starve each other's download
to a crawl. If progress stalls:

```bash
ps ax -o pid,etime,command | grep '[l]imactl start'   # expect exactly one
```

Kill all but the oldest, or watch the real progress directly:

```bash
watch -n5 'ls -la ~/Library/Caches/lima/download/by-url-sha256/*/data.tmp* 2>/dev/null'
```

Then, inside the guest — these are the three checks that distinguish a working
KVM host from a present-but-useless device node *(unverified)*:

```bash
limactl shell fc -- ls -l /dev/kvm
limactl shell fc -- sh -c 'dmesg | grep -iE "EL2|EL1|kvm"'
limactl shell fc -- sh -c 'zcat /proc/config.gz 2>/dev/null | grep -i "CONFIG_KVM="'
```

Expect a device node, a line saying the CPUs started at **EL2** (not EL1), and
`CONFIG_KVM=y` or `=m`. If `/dev/kvm` exists but the guest booted at EL1, nested
virt did not actually engage — recheck `vmType`.

You need write access to it:

```bash
limactl shell fc -- sudo usermod -aG kvm "$USER"   # then restart the shell
```

## 3. Firecracker in the guest *(unverified)*

Pin the version — the guest-side addresses Akuma hardcodes as its bootstrap map
come from a specific release (`src/platform.rs` documents which):

```bash
limactl shell fc -- bash -c '
  V=v1.16.1
  ARCH=aarch64
  curl -L -o /tmp/fc.tgz \
    https://github.com/firecracker-microvm/firecracker/releases/download/$V/firecracker-$V-$ARCH.tgz
  tar -xzf /tmp/fc.tgz -C /tmp
  sudo install -m0755 /tmp/release-$V-$ARCH/firecracker-$V-$ARCH /usr/local/bin/firecracker
  firecracker --version
'
```

**v1.16.1** is what `src/platform.rs`'s Firecracker constants were read from. If
you pin a different version, re-read `src/vmm/src/arch/aarch64/layout.rs` and
`gic/gicv3/mod.rs` and update that file — the memory map has moved between
releases before.

## 4. Build Akuma for Firecracker *(verified through the objcopy step)*

Firecracker's loader wants a **flat binary with an ARM64 Image header**, not the
ELF that `cargo build` emits:

```bash
cargo build --release --features platform-firecracker
rust-objcopy -O binary target/aarch64-unknown-none/release/akuma akuma-fc.bin
```

Verify the load address before booting — this is the single most likely thing to
be wrong, and it fails silently as a hang rather than an error:

```bash
nm target/aarch64-unknown-none/release/akuma | grep ' _boot$'
```

Expect `0000000080300000 T _boot`. The arithmetic that has to hold:
Firecracker loads at `get_kernel_start()` = `0x8020_0000`, then adds the Image
header's `text_offset` = `0x10_0000`, giving `0x8030_0000`. If `_boot` is at
`0x4010_0000` you built the QEMU target by mistake.

Header sanity on the flat binary:

```bash
python3 - <<'EOF'
import struct
raw = open('akuma-fc.bin','rb').read()
to, isz = struct.unpack_from('<QQ', raw, 8)
magic = struct.unpack_from('<I', raw, 56)[0]
print(f'text_offset=0x{to:x} image_size=0x{isz:x} magic=0x{magic:x}')
assert magic == 0x644d5241, 'missing ARM64 Image magic'
assert isz != 0, 'image_size 0 makes the loader assume text_offset=0x80000'
print('load address =', hex(0x80200000 + to))
EOF
```

*(verified: `text_offset=0x100000 image_size=0x394000 magic=0x644d5241`, load
address `0x80300000`.)*

## 5. Why not Docker *(verified — do not retry)*

Docker Desktop's Linux VM cannot host Firecracker, for two independent reasons:

```
$ docker run --rm --privileged alpine zcat /proc/config.gz | grep -i virtualiz
# CONFIG_VIRTUALIZATION is not set        <- KVM compiled out of the kernel
$ ... dmesg | grep -i EL
[0.005578] CPU: All CPU(s) started at EL1  <- no EL2 to run a hypervisor in
```

The kernel is a baked-in blob in the app bundle with no supported replacement,
and Docker VMM (`libkrun.dylib`) never requests nested virt. Fixing one does not
help.

## 6. Boot it *(unverified)*

Copy the image in and write a config. Minimum viable, one vCPU, serial console
only:

```bash
limactl copy akuma-fc.bin fc:/tmp/akuma-fc.bin
limactl shell fc -- bash -c 'cat > /tmp/akuma.json <<EOF
{
  "boot-source": {
    "kernel_image_path": "/tmp/akuma-fc.bin",
    "boot_args": "console=ttyS0"
  },
  "machine-config": { "vcpu_count": 1, "mem_size_mib": 512 }
}
EOF
rm -f /tmp/fc.sock
firecracker --api-sock /tmp/fc.sock --config-file /tmp/akuma.json'
```

**Start with `vcpu_count: 1`.** Akuma's compile-time bootstrap map assumes one
vCPU, because Firecracker places the GIC redistributors at
`0x3FFF_0000 - vcpu_count * 0x2_0000` — CPU0's frames move with the count. The
FDT-derived refinement that makes `SMP=N` correct is **not implemented yet**
(`proposals/FIRECRACKER_PORT.md` §5), so more than one vCPU is expected to lose
the boot core's timer interrupt.

`boot_args` is passed but Akuma ignores the kernel command line; it is harmless.

No drive is configured above, so this only tests boot and console. Add one once
the console works:

```json
"drives": [{ "drive_id": "rootfs", "path_on_host": "/tmp/disk.img",
             "is_root_device": true, "is_read_only": false }]
```

## 7. Verify

In order of what each step proves. Anything past the first failure is not
meaningful.

1. **Firecracker accepts the image.** No `InvalidImageMagicNumber` or
   `InvalidBaseAddrAlignment` on stderr. Failure here is the Image header, not
   Akuma — recheck §4.
2. **The console prints.** Expect, as the first lines:
   ```
   Akuma Kernel starting...
   [Platform] firecracker device map installed
   ```
   The second line is the one that matters: it means the boot assembly mapped the
   PL011 at `0x4000_2000` correctly and the runtime device map was installed. If
   line 1 appears and line 2 does not, the failure is in
   `mmu::rebuild_boot_device_table`. If **nothing** prints, the UART PA is wrong
   or the kernel was linked at the wrong base.
3. **RAM is detected from the FDT.** Expect
   `[Memory] Detected from DTB: base=0x80000000, size=512 MB`. A base of
   `0x40000000` means the FDT was not read and the fallback was used.
4. **The GIC comes up.** Expect `GIC initialized`. This is the first thing that
   depends on the redistributor address, and therefore the first place a wrong
   `vcpu_count` assumption shows up.
5. **The boot suite runs.** `grep -ac PASSED` on the log. Note the count is
   expected to differ from the QEMU figure (289 PASSED / 0 FAILED as of
   2026-08-21) because ~20 assertions in `src/tests.rs` bake in QEMU virt's
   `0x4000_0000..0x8000_0000` as "kernel RAM". Under Firecracker that range is the
   MMIO window: some of those fail loudly, and — the real hazard — others **pass
   vacuously** while covering nothing. Reworking them against
   `akuma_exec::mmu::ram_base()` / `kernel_va_end()` is outstanding.

## 8. Known-outstanding, so you do not debug them twice

- **`SMP=N > 1` is expected broken** under Firecracker. See §6.
- **The FDT device map is not implemented.** Akuma uses the compile-time
  bootstrap map from `src/platform.rs`. Correct for a single-vCPU microVM only.
- **No networking in the config above.** A tap device and a
  `network-interfaces` entry are needed, and `VIRTIO_MMIO_SPI_BASE` is 32 on
  Firecracker versus 48 on QEMU (handled in `src/main.rs`, untested).
- **`src/tests.rs` map assertions** — see §7 step 5.

---

## Background

- `proposals/FIRECRACKER_PORT.md` — verified constants, the vCPU-dependent GIC
  redistributor, and why the device map has to be FDT-derived.
- `docs/archive/GICD_IROUTER_ALIASING.md` — the distributor-span bug found while
  scoping this; fixed at the VA-layout level in `akuma_primitives::addr`.
- `docs/archive/PORTING_POSSIBILITIES.md` — the original options survey.
