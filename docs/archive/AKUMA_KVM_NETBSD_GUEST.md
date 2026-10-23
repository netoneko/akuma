# NetBSD 11.0 amd64 as a guest of Akuma's hypervisor (2026-10-23)

**Result: NetBSD's `MICROVM` kernel boots on the ryzen, on bare metal, under our SVM hypervisor, unmodified,
and reaches its root-device prompt.** No new guest type, no IOAPIC, no hypervisor change was needed to get that
far. What is missing is a root filesystem.

An earlier note in `KVM_BKL_EXIT_HANDLING_AND_BULK_WEDGE.md` § 3 guessed that NetBSD needed a `Guest::NetBsd` and
an IOAPIC. That was a guess from reading the kernel's device list; it is wrong for `MICROVM` (see below).

## What was run

| | |
|---|---|
| kernel | `netbsd-MICROVM.gz` from `https://cdn.netbsd.org/pub/NetBSD/NetBSD-11.0/amd64/binary/kernel/` (1 633 KiB gz, 9 327 648 bytes unpacked), SHA512 checked against that directory's `SHA512` |
| also fetched | `netbsd-GENERIC` (30 097 456 bytes, PCI/ACPI class; not tried) |
| entry | the ELF's Xen note type 18 (`PHYS32_ENTRY`) = PVH entry `0x20b460` (`start_pvh`); segments linked at `0xffffffff80200000`, `p_paddr` `0x200000` |
| where | ryzen (Ryzen 7 8845HS), GRUB entry 30 (`kvmctl kvmmem=4096`, `smp=8`), kernel `158ed98d` |
| host files | `/boot/netbsd-microvm` on p3; herd conf `/etc/herd/available/vm-netbsd.conf`, enabled by symlink |

```
vm = true
vm_type = kvm
vm_kernel = /boot/netbsd-microvm
vm_ram_mib = 512
vm_vcpus = 1
vm_cores = 5
vm_cmdline = console=com -v
restart = false
```

The hypervisor builds the guest's command line as `nosmp virtio_mmio.device=4K@0xd0001000:6` + the spec's
words (`spec_cmdline`, `amd64/src/kvm/linux.rs`): NetBSD's `MICROVM` kernel parses the same `virtio_mmio.device=`
syntax Linux does, and `nosmp` is harmless to it.

## What the guest printed (console, via `/var/log/herd/vm-netbsd.log`)

```
NetBSD 11.0 (MICROVM) #0: Thu Jul 30 15:23:12 UTC 2026
total memory = 511 MB
avail memory = 485 MB
timecounter: Timecounter "i8254" frequency 1193182 Hz quality 100
cpu0: AMD Ryzen 7 8845HS w/ Radeon 780M Graphics , id 0xa70f52
isa0 at mainbus0
com0 at isa0 port 0x3f8-0x3ff irq 4: ns8250 or ns16450, no fifo
com0: console
pv0 at mainbus0
virtio0: kernel parameters: nosmp virtio_mmio.device=4K@0xd0001000:6 console=com -v
virtio0: VirtIO-MMIO-v2
virtio0: network device (id 1, rev. 0x01)
vioif0 at virtio0: features: 0x100000020<V1,MAC>
vioif0: Ethernet address 52:54:00:12:34:56
timecounter: Timecounter "clockinterrupt" frequency 100 Hz quality 0
timecounter: Timecounter "TSC" frequency 4077225480 Hz quality 3000
boot device: <unknown>
root device:
```

So the platform we already model was enough: the 16550 at `0x3f8` (IRQ 4 on the PIC), the PIT as the first
timecounter and the 100 Hz tick (IRQ 0, vector 0x20), the TSC (the guest calibrates it from the PIT and our
CPUID), `virtio-mmio` **v2** with the net device at `0xD0001000` IRQ 6, and the CPUID/MSR filtering. It did not
need an IOAPIC or a local APIC (it runs `nosmp`).

## Things that were not obvious

* **One VM at a time.** `/dev/kvm0` holds one VM; a second `vm = true` service is refused with errno 16
  (`[vm-run] the kernel refused the spec (errno 16)`, EBUSY). With `vm-netbsd` and `vm-serve-mc` both enabled the
  one herd started first got the slot; the other was refused and (with `restart = true`) retried forever. To run
  NetBSD: `herd stop vm-serve-mc`, wait ~35 s (herd caches the parsed conf), `herd start vm-netbsd`; and back.
* **`console=com` is required.** Without it the guest booted and ran (99% guest time, ~300 port-I/O exits/s, IRQ 0
  at 100 Hz, syscall MSRs programmed) and printed *nothing*: the first start had `vm_cmdline = -v`. The kernel
  config's own example is `-append "root=ld0a console=com rw -z"`.
* **A pure "irq vector 20 taken at rip=..." line is not a spin.** `Xhandle_legacy0+0x17` is the IRQ 0 handler's
  entry (the hypervisor logs the first few injected vectors with the RIP at acceptance).
* **`[ Kernel symbol table missing! ]`** is cosmetic: the PVH loader maps the two `PT_LOAD` segments, not the ELF's
  symbol table, which `ddb`/`kvm_mkdb` would use.

## Next

1. **A root disk.** `root=ld0a` wants a virtio block device (`ld`): the hypervisor already offers one at
   `0xD0000000` IRQ 5 when the spec has a disk (`spec_cmdline` then prepends `virtio_mmio.device=4K@0xd0000000:5`
   as well as the NIC's). Build an FFS image from NetBSD's sets (`base.tar.xz`, `etc.tar.xz`, ... from
   `.../NetBSD-11.0/amd64/binary/sets/`) into a RAM-copied disk first (`vm_disk_ram = true`), then a few GB
   squeezed from the Gentoo partition (p7) as an in-place NVMe partition (`vm_disk = /dev/nvme0n1p<N>`).
   NetBSD's `makefs`/`mkfs` can write FFS; a Linux-side `mkfs.ffs` is not standard, so build it on a NetBSD
   guest or with `makefs` from pkgsrc/`nbmake`. `netbsd-INSTALL` embeds a ramdisk but is GENERIC-class (PCI).
2. **Networking.** `vioif0` is up; with `vm_outbound = true` the guest gets DHCP from the gateway
   (`10.0.2.15`, router `10.0.2.2`, DNS `10.0.2.3`) and the outbound path of `KVM_OUTBOUND_NETWORKING.md` applies
   unchanged. Not run yet (needs a userland).
3. `vm_vcpus = 2`: untried; the guest was `nosmp` (our `Platform` offers the MADT only via ACPI, which the Akuma
   spec guest builds; NetBSD `MICROVM` has no ACPI).

## Reproduce

On the ryzen's Pop OS (staging only: nothing here runs nested; the VMs run under Akuma on bare metal):

```sh
mkdir -p ~/netbsd-kernels && cd ~/netbsd-kernels
B=https://cdn.netbsd.org/pub/NetBSD/NetBSD-11.0/amd64/binary/kernel
curl -fsSLO $B/SHA512 && curl -fLO $B/netbsd-MICROVM.gz
grep -F '(netbsd-MICROVM.gz)' SHA512      # compare with: sha512sum netbsd-MICROVM.gz
gunzip -k netbsd-MICROVM.gz
# as root: mount /dev/nvme0n1p3 /mnt/p3; cp netbsd-MICROVM /mnt/p3/boot/netbsd-microvm
#          write /mnt/p3/etc/herd/available/vm-netbsd.conf (above); ln -s into enabled/; arm entry 30
```

The 660 MB `NetBSD-11.0-amd64.iso` also works as a source (`amd64/binary/kernel/` inside it holds the same
files); over the ryzen's wifi it ran at ~210 KB/s, so the kernels were fetched directly instead.
