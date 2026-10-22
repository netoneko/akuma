# KVM: the SVM hypervisor, its Linux guest and its Akuma guest

An AMD SVM (AMD-V) + nested-paging hypervisor inside the amd64 kernel, developed
on the ryzen metal it runs on. It runs a real Linux guest (Firecracker CI
`vmlinux-6.1.128`) with a console, timer interrupts and a virtio-blk ext2 root, and
the **Akuma amd64 kernel itself** as a guest (local APIC, virtio from the command
line, busybox init).
Source: **`amd64/src/kvm.rs`** (phases 0-5, the consistency ladder),
**`amd64/src/kvm/linux.rs`** (the Linux phase), and the pure models in
**`crates/akuma-svm-amd64`**.

> **Stability: C (active risk).** Phases 0-9 have passed on the metal (2026-10-22:
> Linux boots from a virtio-blk root on the ryzen, entry 28; so does Akuma's own
> kernel). The guest cannot reach any
> real device — NPT maps only its RAM and every port and MSR is intercepted — so a
> wrong guest is a stopped run, not a hardware hazard. A wrong *host* side can
> still wedge the box (a `#VMEXIT` leaves GIF clear: `stgi` must follow `vmrun`).

**Licence rule (binding):** no Linux or KVM source may be read. Everything here is
from the AMD APM Vol 2/3, the virtio and ACPI specifications, Xen's public
`start_info.h`, the 8254/8259/16550 datasheets and the guest's own logs.

## Division of labour

`akuma-svm-amd64` is `#![forbid(unsafe_code)]`, `no_std`, no dependencies, and owns
*vocabulary and decisions*. `amd64/src/kvm*.rs` owns every `unsafe` (the MSR
writes, `VMRUN`, `VMSAVE`/`VMLOAD`) and every effect.

| module | what it is |
|---|---|
| `ctrl`, `save`, `intr0/1`, `excp`, `eventinj`, `exitinfo`, `Vmcb` | VMCB layouts and bits, each pinned to its APM table by a test |
| `cpuid::filter` | what the guest may see (no hypervisor bit, no XSAVE/AVX/x2APIC/APIC/MTRR/SVM…) |
| `npt`, `tables` | nested page-table encodings; the guest's own long-mode page walk |
| `elf`, `pvh` | `PT_LOAD` and the Xen PVH entry note; `hvm_start_info` v1 + memory map (+ ACPI entry, `rsdp_paddr`) |
| `devices::{Uart, MsrStore}` | 16550 (incl. the THRE interrupt) and remembered MSRs |
| `pit`, `pic` | 8254 as a function of `now` (1.193182 MHz ticks); 8259A pair, edge-triggered |
| `lapic` | xAPIC register page + timer (periodic/one-shot, 1 GHz bus clock), IRR/ISR with priority classes, self-IPI; a function of `now` |
| `mmio` | decodes the `MOV` — and read-only `CMP`/`TEST` — of a faulting device access (nested page faults give the address only), with the flag arithmetic |
| `virtio` | virtio-mmio **v2** block device over the traits `GuestMem` and `Disk` |
| `acpi` | RSDP v2 / XSDT / FADT / MADT / DSDT with one `LNRO0005` device; `Pm1` registers |

## Invariants

- **`stgi` in the same `asm!` as `vmrun`.** `#VMEXIT` leaves GIF clear; without it
  the host gets no interrupts and the watchdog resets the metal.
- **Interrupts are virtual.** Before each `VMRUN` the loop polls the PIT, raises
  IRQ0, and writes `V_INTR = ctrl::v_intr_extint(vector)` (`V_INTR_MASKING |
  V_IRQ | V_IGN_TPR | vector << 32`). The hardware holds it until the guest's `IF`
  is set and it is out of an interrupt shadow, and writes `V_IRQ` back cleared
  when it dispatches; the first exit afterwards performs the PIC acknowledge,
  before anything else is looked at. No interrupt-window logic.
- **`V_INTR_MASKING` is always on and the loop runs with host IRQs enabled**, so a
  guest running with `IF` clear cannot starve the host tick.
- **The guest's TSC rate is told, not measured.** A PIT calibration loop is a loop
  of VM exits and measures the exits. The command line carries `tsc_early_khz=`
  from `lapic::tsc_hz()`.
- **A device is discoverable only through ACPI.** The guest kernel is built without
  `VIRTIO_MMIO_CMDLINE_DEVICES`. **Never set FADT `HW_REDUCED_ACPI`**: Linux then
  has no legacy PIC/PIT by definition, drops both, and the device's interrupt is
  "not found" (tell-tale: `NR_IRQS … nr_irqs: 8, preallocated irqs: 0`).
- **One offer, one accept.** `Platform::offer(now)` polls every source and returns the
  vector for the guest; `accept` is the INTA/acknowledge once the processor clears
  `V_IRQ`. With the local APIC on (the guest set `IA32_APIC_BASE` bit 11) it is the
  only source; without it, the PIC. A new interrupt source goes through these two,
  never straight into `V_INTR`.
- **The APIC page is emulated only while the guest has it enabled**, at the base the
  guest wrote. A guest told `nolapic` never sets the bit and a touch of that page stays
  a fatal nested page fault naming the address.
- **A device access is not always a `MOV`.** LLVM folds a volatile load used only by a
  comparison into the `CMP`/`TEST`. `mmio::decode` covers those (they read once and
  write only flags); read-modify-write forms are refused.
- **The Akuma guest needs ≥ 768 MiB** (its fixed 512 MiB heap must sit in one region)
  and finds its disk from `virtio_mmio.device=` on the command line, not ACPI.
- **The guest never gets the host's root.** Its disk is the machine's *second*
  `akuma_virtio::block` device, else the preallocated file `/boot/guest-root.img`.
- **Unknown device instructions stop the run.** `mmio::decode` answers `None` for
  anything but the `MOV` family; guessing would corrupt state silently.
- **Pin layouts against an independent consumer**, not the writer: PVH blobs are
  read back with `akuma-ryzen-amd64`'s `StartInfo`, ACPI with its table walker,
  `MOV` encodings come from clang, the disk path from host `md5`/`debugfs`/`e2fsck`.

## Run it

Runbook: [`../../runbooks/kvm-linux-guest.md`](../../runbooks/kvm-linux-guest.md).
Kernel command line: `kvmtest kvmlinux` or `kvmtest kvmakuma` (add `noladder` under
TCG). `kvmlinux` runs the Akuma guest instead when p3's `/boot/kvm-guest` begins with
`akuma` — the bootloader entry's command line is baked into `grubx64.efi`, so the file
is the lever that lets one entry serve both.

## Background

[`../../archive/AKUMA_KVM.md`](../../archive/AKUMA_KVM.md) (the phases and their
timeline), [`../../archive/AKUMA_KVM_LINUX_GUEST.md`](../../archive/AKUMA_KVM_LINUX_GUEST.md)
(the Linux phase, phases 7-8, every bug found and how),
[`../../archive/AKUMA_KVM_AKUMA_GUEST.md`](../../archive/AKUMA_KVM_AKUMA_GUEST.md)
(Akuma as the guest: the local APIC, the `CMP` decoder gap, results).
