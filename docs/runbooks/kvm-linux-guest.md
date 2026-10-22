# Run a guest (Linux, or Akuma itself) under Akuma's hypervisor

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

Linux with the local APIC: `FLAG=kvmlapic ROOT=… GUEST=… sh scripts/kvm/linux_rehearse.sh LOG`
(`[KVM] linux-apic guest: PASS`).

## 3. The Akuma guest (this kernel booting under itself)

```sh
# KERNEL = the amd64 build above (it is both host and guest); BUSYBOX = static x86_64
scripts/kvm/akuma_rehearse.sh /tmp/ak.log        # builds both images, runs QEMU TCG
SELECTOR=1 scripts/kvm/akuma_rehearse.sh /tmp/ak.log   # as ryzen's entry 28 does
```

Expect `[KVM] akuma guest: PASS (banner=yes, init=yes)`, the guest's own console
(`blk: 1 virtio slot(s)`, `fs: ext2 mounted on /dev/vda`, `AKUMA-GUEST-DONE`), and
the script's host-side `md5` of the guest-written `/wtest` plus a clean `e2fsck`.

On the ryzen (build there): the guest kernel is any PVH build of the kernel, at
`/boot/akuma-guest`; the guest disk is the 16 MiB image from
`scripts/kvm/akuma_guest_rootfs.sh` at `/boot/akuma-guest-root.img`; and either boot
entry 29 (needs a `grubx64.efi` with it) or write `akuma` to `/boot/kvm-guest` and boot
entry 28. Delete `/boot/kvm-guest` to get the Linux guest back (`GUEST=linux sh overlays/ryzen/kvm-akuma-stage.sh` does it, `GUEST=linux-apic` selects Linux with the local APIC).

The smallest root: `HELLO=1 scripts/kvm/akuma_rehearse.sh LOG` rehearses the guest whose
only program is `/bin/hello` (`[KVM] akuma-hello guest: PASS`, and `Hello, world!` on the
guest console). On the ryzen build the image there
(`sh scripts/kvm/akuma_hello_rootfs.sh $W/out/akuma-hello-root.img`) and stage with
`GUEST=hello sh overlays/ryzen/kvm-akuma-stage.sh`.

Two virtual processors: `SMP=1 scripts/kvm/akuma_rehearse.sh LOG` (`kvmtest kvmsmp`, or
`akuma-smp` in `/boot/kvm-guest`; on the ryzen `GUEST=smp sh overlays/ryzen/kvm-akuma-stage.sh`)
runs the busybox guest with a two-CPU MADT. Expect `[KVM] linux: cpu 1 started at 00008000`,
the guest's `smp:  2 cpus online` and `guest cpus: 2`, and
`[KVM] akuma-smp guest: PASS (banner=yes, init=yes, smp=yes)`; the summary line carries
`vcpus=2 ipi= init=1 sipi=1 pause=` and each processor's final `rip`/state.

## If it goes wrong

| you see (in the guest's console) | it is |
|---|---|
| `tsc: Unable to calibrate against PIT` | the command line lost `tsc_early_khz=` (`lapic::tsc_hz()` was 0) |
| `virtio-mmio LNRO0005:00: error -ENXIO: IRQ index 0 not found` | FADT flagged hardware-reduced, or the interrupt descriptor is missing: look for `nr_irqs: 8, preallocated irqs: 0` |
| `Waiting for root device /dev/vda...` forever, no `virtio_blk` line | no ACPI node: the guest has no disk (`[KVM] linux: no guest disk`) or `noacpi` is on the command line |
| init prints one character and stops | the UART lost its THRE interrupt (IRQ4 / `OUT2`) |
| `[KVM] linux: … instruction the MMIO decoder does not know` | the guest used a non-`MOV` access on the virtio window; add the encoding to `mmio` with a clang-assembled vector |
| Akuma guest: `[FATAL] heap ends … but the region holding it ends …` | guest RAM below the kernel's fixed 512 MiB heap + image (768 MiB is the floor used) |
| Akuma guest: `…touched the local APIC page with an instruction the MMIO decoder does not know` | a non-`MOV`/`CMP`/`TEST` access on the page; add the encoding to `mmio` with a clang vector |
| Akuma guest: stops with `guest halted waiting for an interrupt nothing can raise` | the guest halted with `IF` set and no unmasked timer: its LAPIC timer LVT is masked or the count is 0 |
| `#UD` in `x86_perf_event_update` at reboot | the NMI watchdog's perf event; `nmi_watchdog=0` (cause of the `RDPMC` fault not understood) |

## Verify

`[KVM] linux guest: PASS` (or `[KVM] akuma guest: PASS (… init=yes)`), the guest's `md5sum /bin/busybox` equals the host's
`md5` of the same file, and the file the guest wrote (`/wtest`) dumps from the
image under host `debugfs` with the md5 the guest printed and `e2fsck -fn` clean.

## Background

[`../archive/AKUMA_KVM_LINUX_GUEST.md`](../archive/AKUMA_KVM_LINUX_GUEST.md) · [`../archive/AKUMA_KVM.md`](../archive/AKUMA_KVM.md)
