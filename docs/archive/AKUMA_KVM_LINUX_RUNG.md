# The Linux rung: a real `vmlinux` under the SVM hypervisor

**Date:** 2026-10-22. **Status:** banner reached under QEMU TCG (`[KVM] linux guest:
PASS (banner=yes)`); **not yet run on metal.** Continues
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
hazard. What it does *not* have: a timer. Linux calibrates against the PIT,
fails (`tsc: Unable to calibrate against PIT`), and spins in its first delay
loop — which is where the quiet budget ends the run. PIT/LAPIC-timer emulation
and interrupt injection are the next rung.

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

## Still open

- Timer: PIT (ports 0x40-0x43, 0x61) then interrupt injection (`EVENTINJ`
  type 0), then the LAPIC timer — until then Linux stops at its first delay loop.
- Metal: not run. Under QEMU/KVM nesting the rung is rehearsed on ryzen first.
- The ladder's last row under TCG (an emulation artefact; metal passes).
- `unsafe`: `amd64/src` 82 → 87 (the trampoline, its call, the host `VMSAVE`,
  and two call sites); budget updated in the same commit.

## Background

[`AKUMA_KVM.md`](AKUMA_KVM.md) · `crates/akuma-svm-amd64` · `amd64/src/kvm/linux.rs` ·
`overlays/ryzen/README.md` (the one-shot drill) · APM Vol 2 §15.10-15.12,
§15.20-15.21, Table B-1/B-2; Vol 3 Appendix E.
