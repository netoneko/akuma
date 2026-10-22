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

A NIC: `NET=1 scripts/kvm/akuma_rehearse.sh LOG` (`kvmtest kvmnet`, or `akuma-net` in
`/boot/kvm-guest`; on the ryzen `GUEST=net sh overlays/ryzen/kvm-akuma-stage.sh`) gives the
busybox guest a virtio-net NIC on the hypervisor's own gateway. Expect the guest's
`eth0 ... inet addr:10.0.2.15  Bcast:10.0.2.255  Mask:255.255.255.0`, and
`[KVM] akuma-net guest: PASS (banner=yes, init=yes, net=yes)` with `dhcp_offer=1 dhcp_ack=1
arp=` in the summary. `NETPROBE=1` adds a UDP-echo probe (`nc -u -w 2`, `udp_echo=1`); under
TCG its 2 s wait costs many minutes of wall time and the guest then stalls after the timeout
(`docs/archive/AKUMA_KVM_AKUMA_GUEST.md` § A NIC), so it is opt-in: `GUEST=netprobe` stages it, `GUEST=net` does not.

**ssh into the guest:** `scripts/kvm/serve_rehearse.py LOG` (TCG; `--hold` leaves it running)
boots the host Akuma with a user-mode NIC forwarding `localhost:2223`, waits for the guest's
`AKUMA-GUEST-SSHD-STARTED`, and runs a real `ssh -p 2223` command in the guest. On the ryzen:
`GUEST=serve` stage + `arm.sh 28`; then from the ryzen itself, or the LAN, `ssh -p 2223
root@<host>`. The guest accepts the repo's test key and every key in the ryzen Akuma's
`/etc/sshd/authorized_keys` (the stage script folds them in). The host forward port is
`kvmfwd=` on the command line (2223). A service guest keeps running until the host reboots.

**Gentoo as the guest (`GUEST=gentoo`):** in Pop, `sh overlays/ryzen/kvm-gentoo-image.sh [3G]` builds
`/boot/gentoo-guest.img` on p3 from p7 (read-only; `tar` into a staging copy, because p7 has ext2
errors that make `mke2fs -d` die with `EUCLEAN`; p7 is never written), then `GUEST=gentoo sh
kvm-akuma-stage.sh` and `arm.sh 28`. The hypervisor copies the image into RAM at start (a minute;
writes are gone when the guest stops). OpenSSH answers on the guest's 4444, forwarded to host 2223;
only the keys in p7's `/root/.ssh/authorized_keys` get in. Its init is `/akuma-kvm-init.sh`
(`overlays/ryzen/gentoo/akuma-kvm-init.sh`).

**herd runs the guest (`vm = true`, ryzen entry 30):** boot entry 30 (`kvmctl kvmmem=4096`: SVM armed, 4 GiB
reserved, no guest at boot), then enable **one** of `overlays/ryzen/rootfs/etc/herd/available/vm-serve.conf`
(the busybox guest, sshd on host 2223) or `vm-gentoo.conf` (Gentoo from the RAM-copied image, host 2223 →
guest 4444) by copying it to p3's `/etc/herd/enabled/` (p3 does not take `rootfs/`; the two share the VM slot and the
port). Check: `herd status` lists the service as `running (vm)`; `herd log vm-gentoo` is the guest's console;
`kill` the `herd vm-run` pid and herd restarts it (the guest is stopped within 200 ms of its owner's death, the
new one waits out `EBUSY`); `echo status > /dev/kvm0; herd log vm-gentoo | tail` shows the kernel's view. Rehearse
the same chain under TCG with `scripts/kvm/serve_rehearse.py LOG --herd --stress 3` (the script reads the
guest's console back from herd's log on the host disk and requires `AKUMA-GUEST-SSHD-STARTED` in it). The boot
flags `kvmserve`/`kvmgentoo` and `/boot/kvm-guest` still work as a shim over the same spec.
Reference: [`../reference/subsystems/kvm.md`](../reference/subsystems/kvm.md) § *VMs as services*.

**Virtual processors of a service guest:** `vm_vcpus` (1 to 4; with the boot flags `vcpus=N` in `/boot/kvm-guest`), e.g.
`printf 'akuma-serve vcpus=2' > /mnt/p3/boot/kvm-guest`, or `kvmvcpus=N` on the command line. They
take turns on the one host core the service task is pinned to: no more compute, and measured *worse*
on 2026-10-22 (late SYN-ACKs 7 of 99 against 0 of 105, one 33 s upload stall); leave it at one.

**Stability and speed checks (run against the ryzen, no guest-side tools needed):**
`scripts/kvm/relay_stress.py <ryzen ip>` (three overlapping sessions on the relay's two slots,
connect-and-drop, silent holds, garbage; each followed by a real ssh that must answer) and
`scripts/kvm/relay_bulk.py <ryzen ip>` (timed 4-8 MiB each way through the relay *from the ryzen
itself*, so the slow wifi link to the Mac is out of the measurement; the serve guest only, it needs
the host's machine key). Read the result in `dmesg | grep -a 'KVM\] \(relay\|guest\)'`: `syn_retries`,
`gave_up`, `stale_in` and `rx_starved` say what the relay saw ([`KVM_RELAY_SYN_STALL.md`](../archive/KVM_RELAY_SYN_STALL.md)).
Hostnames: the host is `akuma-ryzen` (`etc/herd/available/hostname.conf`, enabled on p3; its
Gentoo box's `conf.d/hostname` must say the same or OpenRC resets it to `localhost`), the guests
`akuma-kvm` and `akuma-gentoo`.

## If it goes wrong

| you see (in the guest's console) | it is |
|---|---|
| `tsc: Unable to calibrate against PIT` | the command line lost `tsc_early_khz=` (`lapic::tsc_hz()` was 0) |
| `virtio-mmio LNRO0005:00: error -ENXIO: IRQ index 0 not found` | FADT flagged hardware-reduced, or the interrupt descriptor is missing: look for `nr_irqs: 8, preallocated irqs: 0` |
| `Waiting for root device /dev/vda...` forever, no `virtio_blk` line | no ACPI node: the guest has no disk (`[KVM] linux: no guest disk`) or `noacpi` is on the command line |
| init prints one character and stops | the UART lost its THRE interrupt (IRQ4 / `OUT2`) |
| `vm-run` exits 3 / `[vm-run] the kernel refused the spec (errno 22)` | the spec was refused whole: the reason is the `[KVM] kvm0: spec refused: …` line on the host console (an out-of-range `vm_vcpus`, a missing `vm_kernel`, a duplicate `vm_forward`…) |
| `[vm-run] no /dev/kvm0` | the kernel was not booted with `kvmctl` (or `kvmtest`) |
| `[KVM] linux: refused: guest RAM does not fit the kvmmem= reserve` | the reserve is smaller than this VM's RAM (+ its RAM-disk copy); raise `kvmmem=` |
| `[KVM] linux: … instruction the MMIO decoder does not know` | the guest used a non-`MOV` access on the virtio window; add the encoding to `mmio` with a clang-assembled vector |
| Akuma guest: `[FATAL] heap ends … but the region holding it ends …` | guest RAM below the kernel's fixed 512 MiB heap + image (768 MiB is the floor used) |
| Akuma guest: `…touched the local APIC page with an instruction the MMIO decoder does not know` | a non-`MOV`/`CMP`/`TEST` access on the page; add the encoding to `mmio` with a clang vector |
| Akuma guest: stops with `guest halted waiting for an interrupt nothing can raise` | the guest halted with `IF` set and no unmasked timer: its LAPIC timer LVT is masked or the count is 0 |
| ssh into the guest hangs at the banner / the host port refuses; relay line stuck at `connecting … in_flight=1` | a slot's SYN got no SYN-ACK: before 2026-10-22 a reused source port against a stale guest connection, now the SYN is retried from a new port after 4 s and given up on after 4 tries. `syn_retries`/`stale_in` in the relay and `[KVM] guest:` lines; [`KVM_RELAY_SYN_STALL.md`](../archive/KVM_RELAY_SYN_STALL.md) |
| `#UD` in `x86_perf_event_update` at reboot | the NMI watchdog's perf event; `nmi_watchdog=0` (cause of the `RDPMC` fault not understood) |

## Verify

`[KVM] linux guest: PASS` (or `[KVM] akuma guest: PASS (… init=yes)`), the guest's `md5sum /bin/busybox` equals the host's
`md5` of the same file, and the file the guest wrote (`/wtest`) dumps from the
image under host `debugfs` with the md5 the guest printed and `e2fsck -fn` clean.

## Background

[`../archive/AKUMA_KVM_LINUX_GUEST.md`](../archive/AKUMA_KVM_LINUX_GUEST.md) · [`../archive/AKUMA_KVM.md`](../archive/AKUMA_KVM.md)
