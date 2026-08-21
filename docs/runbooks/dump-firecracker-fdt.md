# Dump the device tree Firecracker gives a guest

**Stability: A** — executed end to end on 2026-08-21; the artifacts it produced
are in `docs/reference/firecracker/fdt/`.

Firecracker builds the FDT in-process and has no dump flag, and Akuma's
`platform-firecracker` device map is a compile-time table read out of
Firecracker's *source*. This is how you get the runtime article instead: boot a
Linux guest that exposes `/sys/firmware/fdt`, and read it off the serial console.

Do this when you need to know what a Firecracker microVM's machine actually looks
like — a new Firecracker version, a different vCPU count, an added device — rather
than what its source said last time it was read.

Needs `/dev/kvm`, so an aarch64 KVM host: the `akuma-terraform` metal instance, or
a local Lima VM with nested virtualization
(`docs/runbooks/run-on-firecracker.md` §2).

---

## 1. The scripted path

On the AWS host, all of this already ran during cloud-init:

```bash
cd ../akuma-terraform
bin/tf.sh apply           # ~10 min: 5-8 of that is a .metal instance booting
bin/status.sh -f          # follow the bootstrap
bin/fetch-fdt.sh          # pull the artifacts down to artifacts/
```

To re-dump after changing the vCPU sweep or the device set:

```bash
bin/ssh.sh 'sudo vi /opt/akuma/env'          # FDT_VCPU_COUNTS, FDT_MEM_MIB
bin/ssh.sh 'sudo /opt/akuma/bin/40-dump-fdt.sh'
bin/fetch-fdt.sh
```

## 2. What it does, in case you are doing it by hand

1. **Build a Linux guest.** Alpine's `aarch64` minirootfs into an ext4 image via a
   native chroot (the host is aarch64, so no emulation), plus `linux-virt` for the
   kernel and initramfs.

2. **Unwrap the kernel.** This is the step that is not obvious. Alpine's
   `vmlinuz-virt` is **not** a raw ARM64 `Image` — since `CONFIG_EFI_ZBOOT` it is
   an EFI zboot PE wrapper with the real Image compressed inside:

   ```
   0x00 "MZ"   0x04 "zimg"   0x08 u32 payload_offset
   0x0c u32 payload_size     0x18 char comp_type[4]
   ```

   Firecracker's `linux-loader` validates the ARM64 magic `0x644d5241` at offset
   56 and nothing else, so it rejects the wrapper with
   `InvalidImageMagicNumber`. Inflate the payload (Alpine 3.24.1 uses `gzip` at
   offset 51832) and check the magic on the *result*.

3. **Boot it with `init=` replaced** by a script that mounts `/proc` and `/sys`,
   quiets the console with `dmesg -n 1`, base64s `/sys/firmware/fdt` between
   markers, dumps `/proc/interrupts` and `/proc/iomem`, and calls `poweroff -f`
   (which becomes a PSCI `SYSTEM_OFF`, so Firecracker exits cleanly).

4. **Reassemble host-side**: strip `\r` **before** filtering to the base64
   alphabet, decode, and check the FDT header.

Once per vCPU count. That axis is the point: the GIC redistributor base is
`0x3fff0000 - vcpu_count * 0x20000`, so one dump proves nothing about the others.

## 3. Two traps that cost a cycle each

**Serial console line endings are CRLF.** Filtering to `^[A-Za-z0-9+/=]+$` before
stripping `\r` discards *every* base64 line, because each ends with a `\r` that is
not in the class. The result is an empty decode that looks exactly like a guest
which never dumped anything. `tr -d '\r'` first.

**FDT header magic is big-endian; `od -tx4` is not.** A perfectly good blob reads
back as `0xedfe0dd0` on aarch64 when the check uses word output. Compare bytewise
(`od -An -tx1 -N4` → `d00dfeed`).

## 4. Verify

In order. Nothing past the first failure is meaningful.

1. **The kernel is accepted.** No `InvalidImageMagicNumber` on Firecracker's
   stderr. Failure here is step 2, not the guest.
2. **The guest boots and finishes.** `===AKUMA-DONE` in the console log, followed
   by `reboot: Power down` and `Firecracker exiting successfully. exit_code=0`.
   A missing end marker with a live Firecracker means the dump script hung; the
   stage timeout (`FDT_TIMEOUT`, default 180 s) will end it.
3. **The blob decodes.** `fdt-vcpuN.dtb` begins `d00dfeed` and `dtc -I dtb` renders
   it without complaint. ~3 KB at 1 vCPU, growing with the CPU node count.
4. **The redistributor base steps by `0x20000`.** In `summary.txt`, the `intc`
   node's `reg` across the sweep:

   ```
   vcpu=1   0x3fff0000 0x10000   0x3ffd0000 0x20000
   vcpu=2   0x3fff0000 0x10000   0x3ffb0000 0x40000
   vcpu=4   0x3fff0000 0x10000   0x3ff70000 0x80000
   vcpu=8   0x3fff0000 0x10000   0x3fef0000 0x100000
   ```

   GICD fixed at `0x3fff0000`/64 KiB; GICR base dropping by `0x20000` per vCPU and
   its span growing to match. Anything else means the Firecracker version moved
   the map, and `src/platform.rs` needs re-reading.
5. **virtio INTIDs start at SPI 0.** `virtio_mmio@40003000` carries
   `interrupts = <0x00 0x00 0x01>`, so INTID 32 — not QEMU virt's 48.

---

## Background

- `docs/reference/firecracker/fdt/` — the artifacts this produced, and what they
  confirm
- `docs/reference/firecracker/memory-map.md` — the map they were checked against
- `proposals/FIRECRACKER_PORT.md` §2.1, §5 — why the vCPU axis decides the
  refactor
- `docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md` — the host this runs on
- `docs/runbooks/run-on-firecracker.md` — the local Lima path
