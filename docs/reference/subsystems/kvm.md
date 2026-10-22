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
| `tcp` | one TCP connection as a pure state machine for a **lossless** peer (handshake in both roles, in-order data through lent rings with real windows, FIN/RST; no retransmit timer, no congestion control, no out-of-order queue) |
| `virtio_net`, `gateway` | virtio-mmio v2 **network** device (queues 0/1, 12-byte header, MAC) over `GuestMem` and `NetBackend`; and the backend: a slirp-style gateway (`10.0.2.2`) that answers ARP, DHCP, ICMP echo and UDP echo and drops the rest |
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
- **Host timer at the guest's deadline.** Before each `VMRUN` the loop arms the host's
  one-shot (`lapic::arm_oneshot_in_us`) at `Platform::next_event_us()` when sooner than
  the periodic tick; `on_tick`'s `restore_periodic` brings the periodic tick back. Never
  `restore_periodic` after an ordinary exit: an exit storm would then postpone the host
  tick (and the hardware watchdog's pet) indefinitely.
- **Legacy interrupts with a local APIC arrive through LINT0 ExtINT** (the virtual wire);
  `Lapic::lint0_extint` says whether the PIC may be consulted.
- **The APIC page is emulated only while the guest has it enabled**, at the base the
  guest wrote. A guest told `nolapic` never sets the bit and a touch of that page stays
  a fatal nested page fault naming the address.
- **A device access is not always a `MOV`.** LLVM folds a volatile load used only by a
  comparison into the `CMP`/`TEST`. `mmio::decode` covers those (they read once and
  write only flags); read-modify-write forms are refused.
- **Virtual processors take turns on the host core** (`Guest::AkumaSmp`, 2 vCPUs;
  `MAX_VCPUS` = 4). Each has its own VMCB, **ASID** (the TLB is tagged by it, and two
  processors share one core), `Lapic` (APIC id = index), `IA32_APIC_BASE` and MSR table;
  the PIT/PIC are processor 0's only. After every exit the loop moves on to the next
  runnable processor. An ICR write that names another processor leaves an `Ipi` in the
  sender's `Lapic` outbox, routed *after* the exit's VMCB borrow ends (a STARTUP
  rewrites another VMCB): INIT parks and resets the target, STARTUP starts a parked one
  in real mode at `vector << 12` (`realmode`), fixed raises the vector (not on a parked
  processor). The MADT lists `n` CPUs (`acpi::build_smp`).
- **`PAUSE` is intercepted while more than one processor can run**, so a spin-waiter
  yields the core at once. Without it a waiter burns its whole slice on a lock whose
  holder is the processor that is not running (a convoy of `[bkls>]` lines) and the
  guest's spin-count logic sees stalls no real machine produces (`SWITCH BADFRAME`).
  The exit does **not** advance RIP: step over it like `HLT` (nRIP, else the length).
- **The gateway's TCP is lossless by construction, not by recovery.** `Gateway::pump` *pulls*
  a segment only while the reply queue has room, and the guest's frames reach it
  synchronously, so `tcp` needs no retransmit timer. Its tests drive it with smoltcp (the
  guest's own stack) in both roles: `crates/akuma-svm-amd64/tests/gateway_tcp.rs`. The
  gateway's storage (`gateway::STORAGE`, ~60 KiB: the queue and six 8 KiB rings) is lent by the
  caller from the heap — it is far too big to build on a 32 KiB kernel task stack.
- **A service guest (`kvmserve`, `akuma-serve`) is a background host task**, not a call
  from boot: its port forward lives on the host's network stack, which exists only after
  boot (on the ryzen, after init). `kvm::spawn_service` starts it **pinned to core 0** (the
  core with SVM enabled) once netpoll is up; `late` skips it. Between guest entries it
  `VMSAVE`s the host state again (other tasks run on the core in between and may have moved
  `KernelGsBase`/`FS_BASE`), yields (`sched::yield_now`, so the BKL is dropped for the
  network and sshd), and polls the relay at most every 200 µs. It has no wall-clock, quiet or
  exit budgets; idle, it arms the host one-shot at the guest's next timer and `hlt`s
  (`sched::allow_tick`). The host must have an init that stays alive (herd on the ryzen; the
  TCG rehearsal gives it `busybox sleep`): without one the boot ends in `halt()` and the
  task is never scheduled.
- **The relay** (`kvm/relay.rs`): a listener on host port `kvmfwd=` (2223) whose clients are
  connected, through a gateway forward slot (the *gateway* opens the connection to the guest's
  port 22), to the guest's sshd. Non-blocking throughout; `socket_*` poll the host stack
  themselves. Two slots.
- **The NIC is polled and has no route out.** `Guest::AkumaNet` puts the virtio-net window
  in the slot after the disk's (`0xD0001000`; the guest finds devices as evenly spaced
  `virtio_mmio.device=` windows, in order) and **never raises its interrupt line** (the
  guest has no IOAPIC and polls). Frames the gateway has for the guest are delivered into
  posted receive buffers at the top of every loop turn and after each transmit. The
  gateway is a model, not a route: anything but ARP/DHCP/ICMP-echo/UDP-echo-to-7 is
  counted and dropped.
- **Stepping over an intercepted instruction ends the interrupt shadow**
  (`Vmcb::complete_instruction`). `sti; hlt` intercepts `HLT` inside the shadow; leaving
  the bit set shields the next instruction too, and the `cli` after the halt then runs
  before the interrupt that woke it can be taken (127 915 of 139 967 exits were `HLT`).
- **The Akuma kernel finds ACPI by scanning `0xE0000..0xFFFFF`** and ignores
  `rsdp_paddr`, so an Akuma guest's table page is placed at `0xE0000` (inside the RAM
  entry, no entry of its own); Linux gets `rsdp_paddr` and a type-3 entry.
- **The Akuma guest needs ≥ 768 MiB** (its fixed 512 MiB heap must sit in one region)
  and finds its disk from `virtio_mmio.device=` on the command line, not ACPI.
- **The guest never gets the host's root.** Its disk is the machine's *second*
  `akuma_virtio::block` device, else the preallocated file `/boot/guest-root.img`.
- **A forwarded connection must not depend on one SYN or one 4-tuple.** The gateway's TCP has no
  retransmit timer and the guest polls its NIC, so a forward slot's source port is a rotating
  generation (`FWD_GENS`) and the relay re-sends an unanswered SYN (`SYN_RETRY_US`, `SYN_TRIES`)
  before resetting the client. Incident: [`KVM_RELAY_SYN_STALL.md`](../../archive/KVM_RELAY_SYN_STALL.md).
- **A service guest's disk may be a RAM copy of a file** (`HostDisk::Mem`, `kvmgentoo`): the guest
  never touches a host block device, and its writes die with it. A second NVMe window is not the
  way to give a guest a partition; the driver's single GPT window is p3's on purpose.
- **Unknown device instructions stop the run.** `mmio::decode` answers `None` for
  anything but the `MOV` family; guessing would corrupt state silently.
- **Pin layouts against an independent consumer**, not the writer: PVH blobs are
  read back with `akuma-ryzen-amd64`'s `StartInfo`, ACPI with its table walker,
  `MOV` encodings come from clang, the disk path from host `md5`/`debugfs`/`e2fsck`.

## Run it

Runbook: [`../../runbooks/kvm-linux-guest.md`](../../runbooks/kvm-linux-guest.md).
Kernel command line: `kvmtest kvmlinux`, `kvmtest kvmlapic` (Linux with the local APIC) or `kvmtest kvmakuma` (add `noladder` under
TCG). `kvmhello` is the same guest on a 2 MiB root whose only file is `/bin/hello`.
`kvmsmp` is the busybox guest with two virtual processors; `kvmnet` (`akuma-net`) the one with a NIC. `kvmlinux` runs an Akuma guest instead when p3's `/boot/kvm-guest` begins with
`akuma` (busybox root), `akuma-hello`, `akuma-smp`, `akuma-net`, `akuma-serve`, `akuma-gentoo` or `linux-apic` — the bootloader entry's command line is baked into `grubx64.efi`, so the file
is the lever that lets one entry serve both.

## Background

[`../../archive/AKUMA_KVM.md`](../../archive/AKUMA_KVM.md) (the phases and their
timeline), [`../../archive/AKUMA_KVM_LINUX_GUEST.md`](../../archive/AKUMA_KVM_LINUX_GUEST.md)
(the Linux phase, phases 7-8, every bug found and how),
[`../../archive/AKUMA_KVM_AKUMA_GUEST.md`](../../archive/AKUMA_KVM_AKUMA_GUEST.md)
(Akuma as the guest: the local APIC, the `CMP` decoder gap, results).
