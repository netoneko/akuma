# Run the Linux guest under Akuma's hypervisor

**Stability: C.** Rehearsal on QEMU TCG is routine; nested SVM and the metal follow
the one-shot loop in [`overlays/ryzen/README.md`](../../overlays/ryzen/README.md).
Design and invariants: [`../reference/subsystems/kvm.md`](../reference/subsystems/kvm.md).

## 1. Rehearse locally (no ryzen)

```sh
cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests
# a static x86_64 busybox, e.g. extracted from the amd64 root image:
sh scripts/kvm/guest_rootfs.sh /tmp/guest.img /path/to/busybox
# ROOT = an amd64 root ext2 image that contains /boot/vmlinux
# (Firecracker CI vmlinux-6.1.128; `debugfs -w -R "write vmlinux /boot/vmlinux"`)
ROOT=/path/to/root.img GUEST=/tmp/guest.img sh scripts/kvm/linux_rehearse.sh /tmp/lx.log
```

Expect `[KVM] linux guest: PASS (banner=yes)` and
`guest rootfs: init ran to completion`. The log shows the guest's own console:
`VFS: Mounted root (ext2 filesystem)`, `Run /sbin/init`, `GUEST-ROOTFS-DONE`.

## 2. On the ryzen

Build **on the ryzen** (a Mac-built kernel hangs on that metal). Put the guest
disk at `/boot/guest-root.img` (16 MiB, from step 1) and the kernel at
`/boot/akuma-amd64.verify` (`kinstall <elf> /boot/akuma-amd64.verify`); entry 28
boots `.verify` when present. Rehearse under nested SVM from Pop
(`DISK=nvme sh overlays/ryzen/qemu.sh 28 240 std`), then `arm.sh 28`. Read the
result with `dmesg | grep -a '\[KVM\]'` on the booted Akuma.

## If it goes wrong

| you see (in the guest's console) | it is |
|---|---|
| `tsc: Unable to calibrate against PIT` | the command line lost `tsc_early_khz=` (`lapic::tsc_hz()` was 0) |
| `virtio-mmio LNRO0005:00: error -ENXIO: IRQ index 0 not found` | FADT flagged hardware-reduced, or the interrupt descriptor is missing: look for `nr_irqs: 8, preallocated irqs: 0` |
| `Waiting for root device /dev/vda...` forever, no `virtio_blk` line | no ACPI node: the guest has no disk (`[KVM] linux: no guest disk`) or `noacpi` is on the command line |
| init prints one character and stops | the UART lost its THRE interrupt (IRQ4 / `OUT2`) |
| `[KVM] linux: … instruction the MMIO decoder does not know` | the guest used a non-`MOV` access on the virtio window; add the encoding to `mmio` with a clang-assembled vector |
| `#UD` in `x86_perf_event_update` at reboot | the NMI watchdog's perf event; `nmi_watchdog=0` (cause of the `RDPMC` fault not understood) |

## Verify

`[KVM] linux guest: PASS`, the guest's `md5sum /bin/busybox` equals the host's
`md5` of the same file, and the file the guest wrote (`/wtest`) dumps from the
image under host `debugfs` with the md5 the guest printed and `e2fsck -fn` clean.

## Background

[`../archive/AKUMA_KVM_LINUX_GUEST.md`](../archive/AKUMA_KVM_LINUX_GUEST.md) · [`../archive/AKUMA_KVM.md`](../archive/AKUMA_KVM.md)
