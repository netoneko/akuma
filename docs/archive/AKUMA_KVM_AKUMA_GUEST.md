# Rung 3: Akuma boots under its own hypervisor

**Date:** 2026-10-22. **Status:** **PASS on the ryzen metal** (entry 28, 2026-10-22) and under QEMU TCG:
`[KVM] akuma guest: PASS (banner=yes, init=yes)`. Nested SVM was skipped (the box was
offline for it; the metal run went straight after TCG). Third rung of the ladder in [`AKUMA_KVM.md`](AKUMA_KVM.md) ("KVM runs
akuma"); continues [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md). No Linux
or KVM source was read; every register offset and bit is the architectural xAPIC
map (AMD APM Vol 2, ch. 16), the x86 `CMP`/`TEST` definitions, or the guest's own
trace.

## What runs

`kvmtest kvmakuma` (or `kvmtest kvmlinux` with `/boot/kvm-guest` beginning `akuma`):
`kvm::late` runs `kvm/linux.rs` with `Guest::Akuma`. The guest is **this kernel's own
ELF**, read from `/boot/akuma-guest`, entered the PVH way exactly as Firecracker
enters it, with:

* 768 MiB of RAM (the kernel takes a fixed 512 MiB heap, `mem.rs` `HEAP_SIZE`, out
  of one region and refuses to boot when the region is smaller — first attempt, at
  256 MiB: `[FATAL] heap ends 0x21191000 but the region holding it ends 0xfffd000`);
* the command line `nosmp virtio_mmio.device=4K@0xd0000000:5 init=/bin/busybox
  initargs=sh,/etc/guest.sh` — a virtio-mmio window announced the Firecracker way, no
  ACPI, no PCI, no network;
* the same COM1 / PIT / PIC / CPUID / MSR platform as the Linux guest, **plus a local
  APIC**;
* a 16 MiB ext2 disk (`scripts/kvm/akuma_guest_rootfs.sh`): busybox and a script that
  prints `AKUMA-GUEST-UP`, `uname -a`, the md5 of busybox, writes and `sync`s a 256 KiB
  file, prints its md5 and `AKUMA-GUEST-DONE`. The hypervisor takes that marker as the
  pass condition and stops.

## What had to exist

**A local APIC** (`akuma_svm_amd64::lapic`, 23 host tests). Akuma's `lapic::init` maps
`0xFEE0_0000`, software-enables it, calibrates its timer against the PIT, and from then
on its tick, its idle one-shots (`arm_deadline`, `arm_idle_shot`), and its EOIs are
LAPIC registers. Linux was told `nolapic`; Akuma cannot be. The model is a function of
`now` like the PIT: xAPIC register page (ID, version, TPR/PPR, EOI, SVR, ISR/IRR/TMR,
ESR, the LVT entries, ICR with the fixed self-IPI), a periodic/one-shot timer with the
documented divide encoding, request/in-service state with priority classes, and
**coalescing**: periods the caller was too late to see are one request and the next edge
keeps its original phase. Delivery is the same virtual interrupt as the PIC's
(`V_INTR`, §15.21.4); the first exit after the processor clears `V_IRQ` performs the
acknowledge, so the guest's EOI (an MMIO store, an exit of its own) always finds
the in-service bit set. The page is emulated only after the guest sets the enable bit of
`IA32_APIC_BASE` — a Linux guest that never does is unchanged.

**`CMP`/`TEST` in the MMIO decoder.** The second stop: Akuma's virtio driver died at
`44 3b 88 fc 00 00 00` = `cmp r9d, [rax+0xfc]` ("config generation unchanged?").
LLVM folds a volatile load whose only use is a comparison into the comparison, so a
device access need not be a `MOV`. `mmio` now decodes `38/39/3A/3B`, `80/81/83 /7`,
`84/85`, `F6/F7 /0` — which only *read* the window and write only the arithmetic flags
— and computes the flags (`sub_flags`, `test_flags`). Read-modify-write ALU forms stay
refused. Pinned against two independent oracles: the encodings are **assembled by
clang**, and the flag arithmetic is checked against **1 600 operand pairs executed on
real x86 `cmp`/`test`** (`scripts/kvm/flag_oracle.c`, run under Rosetta on the Mac;
vectors in `crates/akuma-svm-amd64/src/mmio_flag_vectors.in`).

Small things: the guest's `#PF` is not intercepted (Akuma demand-faults all user memory;
nested paging delivers it with no exit), `RAM`, the disk path and the kernel path are
per-guest, and `kvm::late` picks the guest.

## Results

| where | result |
|---|---|
| QEMU TCG, Mac (`scripts/kvm/akuma_rehearse.sh`) | **PASS**: banner, 34 LAPIC timer interrupts delivered, `blk` found from the command line, ext2 mounted, busybox init ran, 10 516 exits (3 453 MMIO, 48 LAPIC page accesses). Guest `/wtest` md5 `71277886…` = host `debugfs` dump of the image; `e2fsck -fn` clean. Same through the `/boot/kvm-guest` selector |
| Linux guest regression (same kernel, `linux_rehearse.sh`) | PASS, `init ran to completion` — unchanged |
| **metal, ryzen entry 28** (`/boot/kvm-guest` = `akuma`, kernel `b3a2c71` built on the ryzen) | **PASS**: the guest calibrated its LAPIC timer against the virtual PIT (`625062 counts per 10000us`) and measured the real TSC (**3794 MHz**), `blk` from the command line, ext2 mounted, `uname` printed, `md5sum /bin/busybox` = `ebce4301…` (the host's), `/wtest` = `71277886…`, `AKUMA-GUEST-DONE`; **9 853 exits, 24 virtual interrupts, 1 665 MMIO, 38 LAPIC page accesses**. Independent check from Pop: `/boot/akuma-guest-root.img` on p3 was rewritten by the guest (mtime = the boot), `debugfs dump /wtest` → md5 `71277886…`, `e2fsck -fn` clean. Log: [`AMD64_KVMTEST_PHASE9_AKUMA_METAL_DMESG_2026_10_22.txt`](AMD64_KVMTEST_PHASE9_AKUMA_METAL_DMESG_2026_10_22.txt) |

## Still open

* LAPIC timer granularity: a guest spinning with `IF` set sees timer interrupts at the
  host-tick rate (the host tick is the only exit it is guaranteed to produce), HLT idle
  is exact. Fine for a 10 ms guest tick; a finer guest would need the host to arm a
  one-shot at the guest's deadline.
* One vCPU (`nosmp`), no IOAPIC, no virtio-net, no PCI. SMP needs INIT/STARTUP delivery
  and a second VMCB; the `Lapic` model already carries the ICR.
* The guest's `HLT` with `IF` clear stops the run (same as the Linux guest).

## Background

[`AKUMA_KVM.md`](AKUMA_KVM.md) · [`AKUMA_KVM_LINUX_GUEST.md`](AKUMA_KVM_LINUX_GUEST.md) ·
`crates/akuma-svm-amd64/src/{lapic,mmio}.rs` · `amd64/src/kvm/linux.rs` ·
[`../runbooks/kvm-linux-guest.md`](../runbooks/kvm-linux-guest.md)
