# The Linux rung: a real `vmlinux` under the SVM hypervisor

**Date:** 2026-10-22. **Status:** **PASS on the metal** (ryzen, entry 28):
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

- A root filesystem: the guest now panics at `VFS: Unable to mount root fs`. A
  virtio-blk device backed by a host partition (the Gentoo one on ryzen) is the
  natural next rung; it also exercises `HLT` idle.
- LAPIC timer (the guest is told `nolapic` today), nested-SVM and metal runs of
  rung 7.
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
