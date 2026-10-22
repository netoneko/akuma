# The Linux rung: a real `vmlinux` under the SVM hypervisor

**Date:** 2026-10-22. **Status:** **PASS on the metal** through rung 8 (Linux boots from a virtio-blk root on the ryzen, entry 28); rung 6:
`[KVM] linux guest: PASS (banner=yes)` — Linux 6.1.128 printed its banner and ran
on to its first timer wait on real Zen 4 under this hypervisor
([`AMD64_KVMTEST_RUNG6_LINUX_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_RUNG6_LINUX_METAL_DMESG_2026_10_22.txt)).
Also PASS under QEMU TCG and under QEMU/KVM with nested SVM on ryzen. Continues
[`AKUMA_KVM.md`](AKUMA_KVM.md) (rungs 0-4, crush) — this is rung 5 (PVH handoff
dry run, crush's) and rung 6 (the real kernel). No Linux or KVM source was read
(GPL, binding for the project); every layout and bit is from AMD APM Vol 2/3 or
Xen's public `start_info.h`, and the guest's behaviour was learned from its exit
trace.

## What runs

`kvmtest kvmlinux` on the command line. `kvm::init` runs rungs 0-5 and the ladder
early; `kvm::late` (after the root mount — PVH path: inside `boot::late_init`'s
closure; multiboot2 path: right after `have_fs`) runs `amd64/src/kvm/linux.rs`:

1. 128 MiB of guest RAM from `alloc_pages_contiguous_zeroed`, 2-MiB aligned,
   mapped by 2 MiB NPT leaves (guest-physical `g` = host `base + g`). Nothing else
   is mapped — a touch of the APIC, a ROM or a device is an NPF that stops the
   run and names the address.
2. `/boot/vmlinux` (an uncompressed PVH ELF; Firecracker CI `vmlinux-6.1.128`)
   streamed segment by segment through the VFS into that RAM, one frame as the
   read window. PVH entry from the `Xen` type-18 note.
3. `hvm_start_info` v1 + memmap + cmdline under the top of RAM; entry in 32-bit
   protected mode, paging off, EBX = start_info.
4. `vmrun_regs`: loads the guest's whole register file, `VMLOAD` guest, `VMRUN`,
   `VMSAVE` guest, spills registers, `VMLOAD` host (the kernel's per-CPU `%gs`
   survives a guest that sets its own). `V_INTR_MASKING` is set and the run is
   with IRQs **on**, so the host tick (scheduler, hardware watchdog) is never
   starved by a guest running with `IF` clear (§15.21.1).
5. Every CPUID, port and MSR is an exit: `akuma_svm_amd64::cpuid::filter`
   (no hypervisor bit, no XSAVE/AVX/x2APIC/MONITOR/APIC/MTRR/SVM…), a COM1 16550
   model that prints to the host console, masked PICs / empty PCI / zero CMOS,
   VMCB-backed MSRs (EFER, STAR…, SYSENTER_*, PAT, GS/FS base) and a remembered
   table for the rest. `#UD/#GP/#PF/#DF` are intercepted, logged and re-injected
   (CR2 set by the VMM, §15.12.15).
6. Stops on: HLT, triple fault, NPF, unhandled exit, 120 s wall clock, or 20 s of
   console silence after the banner. PASS = the banner arrived and nothing fatal.

Because the guest cannot reach any real device (NPT maps only its RAM; every
port and MSR is intercepted), a wrong guest is a stopped run, not a hardware
hazard. Rung 6 had no timer; rung 7 (below) adds one.

## Rung 7: a PIT, a PIC and interrupt injection (2026-10-22)

`akuma_svm_amd64::{pit, pic}` are pure, host-tested models (24 tests):

* `pit::Pit` — an 8254 as a function of `now` in input-clock ticks (1.193182 MHz,
  `ticks_from_us`). Control word, counter latch, read-back, modes 0-5 (binary
  only), port B (`0x61`) gate/output/refresh-toggle for channel 2, and for
  channel 0 `take_irq0(now)` / `next_irq0_tick()`: output rising edges, coalesced
  when the caller is late, as an 8259 whose IRR bit is already set does.
* `pic::PicPair` — two 8259As: ICW1-4, OCW1-3, edge-triggered, fixed priority,
  fully nested, auto-EOI, the slave on IRQ2. `pending()` is the vector to offer;
  `acknowledge()` is the INTA cycle.

Delivery is the **virtual interrupt** of §15.21.4, not `EVENTINJ`: before every
`VMRUN` the loop polls channel 0, raises IRQ0 and writes
`V_INTR = ctrl::v_intr_extint(vector)` (`V_INTR_MASKING | V_IRQ | V_IGN_TPR |
vector << 32`); the processor holds it until the guest's `IF` is set and it is out
of an interrupt shadow, and writes `V_IRQ` back cleared when it dispatches. The
first exit afterwards sees the bit clear and performs the PIC acknowledge — before
it looks at anything else, so the guest's EOI (an exit of its own) always finds the
PIC up to date. No interrupt-window logic, no `VINTR` intercept: the hardware
already does the "wait until IF" part. `EVENTINJ` type 0 would need both, and
stays unused. `HLT` with `IF` set now advances `RIP` and idles until the PIT can
raise an interrupt (it stopped the run before); `HLT` with nothing armed stops it.
A `0xFE` to port `0x64` (the keyboard-controller reset pulse, which is what
`panic=-1` ends in) is a clean stop.

**The PIT does not calibrate the TSC, and cannot here.** The guest's PIT probe
polls port B bit 5 for 10 ms; every poll is an exit. Measured over 750-770 polls
per window (TCG): largest gap between two polls 5-10x the smallest (a host tick
landing in the loop), and the guest gave up three times with
`tsc: Unable to calibrate against PIT`. A trapped port read measures the exit,
not the clock — which is why real hypervisors tell the guest its TSC rate instead.
The guest's command line now ends `tsc_early_khz=<host TSC kHz>` (documented
kernel parameter; `lapic::tsc_hz()`; assembled on the stack), and the guest
prints `tsc: Detected 998.200 MHz processor`. The PIT model stays: IRQ0 is the
tick, and the delay loop is calibrated from it (`Calibrating delay loop
(skipped), value calculated using timer frequency`).

| where | result |
|---|---|
| QEMU TCG, Mac (`noladder`) | PASS: 144 virtual interrupts taken, `tsc-early` clocksource, runs to `VFS: Unable to mount root fs`, reset pulse recognised, 95 773 exits (was 1.3 M of spinning) |

Not yet exercised: `HLT` idle (the guest panics before it idles — needs a root
filesystem); nested-SVM and metal runs of this rung. Granularity: while the guest
runs, IRQ0 is polled once per exit, and the host tick is the only exit a busy guest
is guaranteed to produce, so a guest that spins with `IF` set sees timer
interrupts at the host-tick rate, not the PIT's. Idle (HLT) is exact.

## Rung 8: a virtio-blk root (2026-10-22)

The guest boots from a virtio-blk disk, mounts its ext2 root, runs busybox `init`
and reboots. Four new pure, host-tested models in `akuma-svm-amd64`, plus the
wiring in `kvm/linux.rs`:

* `mmio` — decodes the `MOV` a guest uses to touch a device (`88/89/8A/8B`,
  `C6/C7`, `0F B6/B7/BE/BF`, `63`; prefixes, REX, ModRM/SIB/disp lengths), because
  a nested page fault names the address and nothing else. 33 test vectors were
  **assembled by clang**, not hand-written. Unknown instructions stop the run.
* `virtio` — a virtio-mmio **version 2** block device: register file, one split
  virtqueue, `IN/OUT/FLUSH/GET_ID`, status/feature negotiation (`VERSION_1`,
  `SEG_MAX`, `FLUSH` offered), bounds/alignment/direction checks, a hostile-ring
  guard (`DEVICE_NEEDS_RESET`). Over two traits, `GuestMem` and `Disk`.
* `acpi` — RSDP v2, XSDT, FADT (rev 6), MADT, DSDT with one
  `Device(VIO0) { _HID "LNRO0005", _UID, _CRS }`, plus `Pm1` (the PM1a status/
  enable/control registers the FADT names). Read back in a test by
  `akuma-ryzen-amd64`'s table walker, the code that parses real firmware.
* `uart` gains the THRE interrupt (IRQ4); `pit`/`pic` unchanged.

**Why ACPI.** The Firecracker CI kernel has `CONFIG_VIRTIO_MMIO=y` and no
`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES` (read from its embedded `IKCFG`), so
`virtio_mmio.device=` is silently ignored; with no virtio-PCI either, a device
is discoverable only as an ACPI `LNRO0005` node. `rsdp_paddr` in `start_info`
carries the RSDP (the kernel honours it; real VMMs leave it 0 and the BIOS area
is scanned instead).

**The trap: do not set FADT `HW_REDUCED_ACPI`.** First version did, and the
guest's own log said what followed: `nr_irqs: 8, preallocated irqs: 0` (a
non-ACPI boot says `24`/`16`). A hardware-reduced platform has no legacy PIC or
PIT by definition, so Linux drops both, never creates IRQ 5, and the device's
interrupt comes back disabled: `virtio-mmio LNRO0005:00: error -ENXIO: IRQ index
0 not found` (after `virtio_blk` had already probed — 23 emulated register
accesses). Setting `IAPC_BOOT_ARCH.LEGACY_DEVICES` does not help; the reduced
flag wins. The FADT is full-hardware with a PM1 block (ports `0x600-0x605`,
`SCI_EN` always set, no `SMI_CMD` — zero means ACPI mode is already on), no PM
timer, no GPE.

Two more guest-visible traps, both from the log:

* **8250 transmit stalls without a THRE interrupt.** Early `printk` polls, but
  once the console driver takes over a `16450` waits for an interrupt after every
  character. Symptom: init's output stops after one byte (`G`). The UART model
  now raises THRE on IER enable and on each character, clears it on an `IIR`
  read, and gates it with `OUT2`.
* **`#UD` on `RDPMC` at reboot.** The only perf event is the NMI watchdog;
  `x86_pmu_del` runs `RDPMC` in `perf_reboot`. `nmi_watchdog=0` removes it.
  (Why `RDPMC` itself faults in this guest is not understood.)

**Backing store: the existing driver.** `HostDisk` is the machine's *second*
`akuma_virtio::block` device (`read_bytes_at`/`write_bytes_at`; the host's own
`vda` is never offered), else a preallocated file `/boot/guest-root.img` through
`akuma_vfs_glue`. Akuma's virtio-blk is the driver role; the new `virtio` module
is the device role, which nothing in the tree had. The ryzen's NVMe driver holds
a single partition window, so a raw Gentoo partition needs a second window in
`nvme.rs` first (open).

**Verified (QEMU TCG), both directions, against independent tools:**

| check | result |
|---|---|
| guest `md5sum /bin/busybox` (1.1 MB read through the device) | `ebce4301…` = the host's `md5` of the same file |
| guest writes a 256 KiB pattern file, `sync` | host `debugfs dump` of the image: identical md5 (`71277886…`), `e2fsck -fn` clean |
| boot | `[vda] 32768 512-byte logical blocks`, `VFS: Mounted root (ext2 filesystem)`, `Run /sbin/init`, init output, `reboot -f`, clean stop; 98 882 exits, 234 interrupts, 126 MMIO accesses |

Tooling (kept in the repo, not the scratchpad): `scripts/kvm/guest_rootfs.sh`
builds the 16 MiB ext2 image from a static busybox with `mke2fs -d` + `debugfs
mknod`; `scripts/kvm/linux_rehearse.sh` runs the QEMU rehearsal and, with a
guest image, requires the guest's `GUEST-ROOTFS-DONE`.

**Nested SVM and the metal (2026-10-22, later).** Built **on the ryzen** (the
Mac-built-kernel hang), kernel as `/boot/akuma-amd64.verify`, the guest disk as the
file `/boot/guest-root.img` on p3 (16 MiB), `/boot/vmlinux` as before. The file
backend had not run anywhere until a TCG pass with the disk inside the host's own
root image; it worked.

| where | result |
|---|---|
| nested SVM, ryzen (`qemu.sh 28`, p3 32 GiB, KVM) | PASS: `tsc: Detected 3789.926 MHz`, `[vda] 32768 blocks`, `VFS: Mounted root (ext2)`, guest `md5sum /bin/busybox` = `ebce4301…` (host's), guest-written `/wtest` = `71277886…`; 100 301 exits, 305 virtual interrupts; the rehearsal's own root `e2fsck` clean |
| **metal, ryzen entry 28** | **PASS**: `tsc: Detected 3793.429 MHz`, `model name : AMD Ryzen 7 8845HS w/ Radeon 780M Graphics` printed by the guest, same two md5s, `reboot: machine restart` → clean stop; **99 869 exits, 207 virtual interrupts, 156 MMIO accesses, 21 host IRQs**; the file `/boot/guest-root.img` was rewritten by the guest (mtime = the boot). Log: [`AMD64_KVMTEST_RUNG8_VIRTIO_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_RUNG8_VIRTIO_METAL_DMESG_2026_10_22.txt) (the kernel's `dmesg` ring had wrapped under wifi chatter; this is `klog-176.all`) |

Honest scope: the guest is a 16 MiB ext2 + busybox, not Gentoo; one vCPU, no
network, no LAPIC. The wifi station then took minutes to join —
[`AMD64_WIFI_JOIN_LATENCY_2026_10_22.md`](AMD64_WIFI_JOIN_LATENCY_2026_10_22.md).

**Not yet:** a raw partition as the
backing store (NVMe second window); the LAPIC timer; queue interrupts are edge-
raised once per `process` (fine for one queue, revisit with more devices).

## Bugs found on the way (all in code that was already "green")

| symptom | cause | how it hid |
|---|---|---|
| `vmrun` did not compile (WIP) | `r15` declared as an input and as a clobber | — |
| PVH entry never found in a real kernel | `PVH_NOTE_TYPE` was 9 (`XEN_ELFNOTE_LOADER`); the entry note is 18 (`PHYS32_ENTRY`); the walker also stepped notes by unpadded sizes | synthetic-image tests built their note from the same constant |
| Linux `ud2` in its PVH setup, no cmdline, no memmap | `hvm_start_info` written at version-0 offsets (cmdline `+0x10`, memmap `+0x20`, 48 B; entry `type@0`): the v1 layout is cmdline `+0x18`, rsdp `+0x20`, memmap `+0x28`, entries `+0x30`, 56 B; entry `addr@0 size@8 type@0x10` | the PVH rung's synthetic guest read the same wrong offsets and passed. Now `pvh_blobs_parse_with_the_kernels_own_start_info_reader` reads our blobs with `akuma-ryzen-amd64`'s `StartInfo` — the parser that boots on QEMU and Firecracker |
| triple fault with no cause | `#UD` delivered through an empty IDT | exception intercepts + re-injection print the first fault and the instruction bytes (guest page-table walk with crush's `tables::walk`) |

Lesson, again: a layout test written by the same hand as the writer pins the
author's reading, not the format. Pin against an independent consumer.

## Rehearse, then metal (the drill)

Local, no ryzen: `FEATURES=no-tests` kernel, then
`qemu-system-x86_64 -M microvm -cpu EPYC-Milan,svm=on -m 2048 -smp 1 -kernel
akuma-amd64 -drive …root.img … -append "… init=/bin/hello kvmtest kvmlinux
noladder"` with `/boot/vmlinux` in the image (`debugfs write`). `noladder`: under
TCG the ladder's last row (LME+LMA, paging off) resets the machine; the metal
answers `entered`.

On ryzen, from Pop: stage the guest kernel once (`curl -o $W/vmlinux
https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.12/x86_64/vmlinux-6.1.128`,
then `mount p3; cp $W/vmlinux /mnt/boot/vmlinux`), `build.sh` (puts `$W/vmlinux`
in the rehearsal image), `DISK=nvme sh overlays/ryzen/qemu.sh 28 240 std` and
`grep -a '\[KVM\]' $W/qemu/serial-28.log` (the script's own verdict greps do
not know `[KVM]`), then `install.sh` (a new `grubx64.efi` carries entry 28),
the built kernel to p3 `/boot/akuma-amd64.verify`, `arm.sh 28`. Entry 28 boots
`.verify` when present (else `akk`), so entries 20/27 and the live kernel are
untouched and rollback is "arm another entry".

## Results

| where | result |
|---|---|
| QEMU TCG, Mac (`noladder`) | PASS, 12 823 exits, 2 012 host IRQs |
| QEMU/KVM nested SVM, ryzen (`qemu.sh 28`, p3 32 GiB) | PASS, 12 923 exits, 2 009 IRQs; all 12 ladder rows `entered` |
| **metal, ryzen entry 28** | **PASS**, 12 964 exits, 2 001 IRQs, 4 reflected exceptions, 5 262 console bytes; `rip=ffffffff81000ee6` in the first delay loop; ASIDs 32768, `nrips=yes`; Akuma then finished booting (wifi, DHCP, SNTP) |

The nested rehearsal earned its keep: it found that on the multiboot2 path
`kvm::late` ran *before* `boot::late_init`'s `lapic::init`, so the host had no
tick — the guest's spin never produced an exit and the wall-clock budget (checked
on exits) never fired. The call now lives in the late-init closure on both paths.
TCG could not show it because its host timer was already running.

## Still open

- The Gentoo partition as the backing store (needs a second NVMe window in
  `nvme.rs`, or a file on p3), then more guest: networking (virtio-net), SMP, the
  LAPIC timer.
- LAPIC timer (the guest is told `nolapic` today).
- p3's `e2fsck -fn` reports a corrupted directory in crush's cargo cache
  (`/root/ktarget/.../build/akuma-amd64/<hash>/out`, last written Oct 18) — not
  caused by this work, not repaired; `rm -r` that directory from Pop, then `e2fsck`.
- The ladder's last row under TCG (an emulation artefact; metal passes).
- `unsafe`: `amd64/src` 82 → 87 (the trampoline, its call, the host `VMSAVE`,
  and two call sites); budget updated in the same commit.

## Background

[`AKUMA_KVM.md`](AKUMA_KVM.md) · `crates/akuma-svm-amd64` · `amd64/src/kvm/linux.rs` ·
`overlays/ryzen/README.md` (the one-shot drill) · APM Vol 2 §15.10-15.12,
§15.20-15.21, Table B-1/B-2; Vol 3 Appendix E.
