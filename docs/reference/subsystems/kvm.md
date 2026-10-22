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

The VM's *description* is a separate leaf crate, **`akuma-vm-spec`** (`forbid(unsafe_code)`, no
dependencies): the `Spec` (kernel, command line, RAM, vCPUs, disk, port forwards, backend) with its
parser, its `key=value` wire text, the first-fit extent `arena` over the reserved guest RAM, and the
`kvmcores=` list parser. The kernel (`amd64/src/kvm/ctl.rs`) and herd both depend on it, so they cannot
disagree on a key or a limit.

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
- **A service guest is the VM task's guest** (see *VMs as services* below), not a call
  from boot: its port forward lives on the host's network stack, which exists only after
  boot (on the ryzen, after init). The VM task is **pinned to one core with SVM enabled** (core 0,
  or the first of `kvmcores=`); the boot flags `kvmserve`/`kvmgentoo` queue the same spec through
  `kvm::spawn_service` once netpoll is up; `late` skips them. Between guest entries it
  `VMSAVE`s the host state again (other tasks run on the core in between and may have moved
  `KernelGsBase`/`FS_BASE`), yields (`sched::yield_now`, so the BKL is dropped for the
  network and sshd), and polls the relay at most every 200 µs. It has no wall-clock, quiet or
  exit budgets; idle, it arms the host one-shot at the guest's next timer and `hlt`s
  (`sched::allow_tick`). The host must have an init that stays alive (herd on the ryzen; the
  TCG rehearsal gives it herd, or `busybox sleep` for the boot flag): without one the boot ends in
  `halt()` and the task is never scheduled.
- **The relay** (`kvm/relay.rs`): a listener on host port `kvmfwd=` (2223) whose clients are
  connected, through a gateway forward slot (the *gateway* opens the connection to the guest's
  port), to the guest's sshd. Non-blocking throughout; `socket_*` poll the host stack
  themselves. One listener per `forward` of the spec (up to four), sharing the gateway's two
  connection slots; `Relay::close` gives every socket back when the guest ends, so a restarted guest
  binds the same ports.
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
- **A service guest's virtual processors come from its spec** (`vm_vcpus`, 1 to 4; for the boot
  flags `kvmvcpus=N`, or `vcpus=N` after the guest word in the selector file): all of them run on
  the one host core the VM task is pinned to, taking turns, so more of them is latency, not compute.
- **A service guest's disk is a file, a RAM copy of a file, or an NVMe partition in place**
  (`vm_disk`, `vm_disk_ram`). A RAM copy (`HostDisk::Mem`) never touches a host block device and its writes die
  with it; the copy is a slice of the reserve and goes back (`DiskLease`) when the run ends, on every path out.
  A partition (`vm_disk = /dev/nvme0n1p<N>`, `HostDisk::Nvme`, 2026-10-22) is used **in place** — its writes are
  the partition's — through a **second NVMe window** (`nvme::open_guest_partition`): validated from the same
  CRC-checked GPT, bounds-checked by the same `Window`/`absolute` path as the host's p3 window, one at a time,
  and **refused when it is the host's partition or overlaps it** (the host's root is never a guest's disk). No
  other raw device can be named (`akuma-vm-spec` rejects `/dev/sda1`, `/dev/nvme0n1`, …), and `disk_ram` with a
  partition is a `Conflict`. (Until 2026-10-22 this paragraph said a second window was deliberately not the
  way to give a guest a partition; that was written before there was a bounds-checked way to do it.)
- **Unknown device instructions stop the run.** `mmio::decode` answers `None` for
  anything but the `MOV` family; guessing would corrupt state silently.
- **Pin layouts against an independent consumer**, not the writer: PVH blobs are
  read back with `akuma-ryzen-amd64`'s `StartInfo`, ACPI with its table walker,
  `MOV` encodings come from clang, the disk path from host `md5`/`debugfs`/`e2fsck`.

## VMs as services: `/dev/kvm0` and herd

**AMD SVM only** (Intel VMX is out of scope). Grade of this section: **C** — passed on the ryzen metal
2026-10-22 (entry 30, `kvmctl kvmmem=4096`; `vm-serve` and `vm-gentoo`, restart by kill and by `stop`).
Runbook: [`../../runbooks/kvm-linux-guest.md`](../../runbooks/kvm-linux-guest.md) § *herd*.

A VM is a herd service. herd turns `vm_*` keys into an `akuma_vm_spec::Spec`, writes its wire text to
`/var/run/herd/<name>.vm` and supervises one ordinary process, `herd vm-run <file>`. That process
opens **`/dev/kvm0`**, writes the spec (`create`), copies the console to stdout (so the guest's console
**is** `/var/log/herd/<name>.log`) and exits when the guest does. The VM runs while that process does.

| operation | how |
|---|---|
| create | `write` a spec to `/dev/kvm0`: parsed whole, refused whole — `EINVAL` (bad spec; the reason is on the host console), `EOPNOTSUPP` (`type=firecracker`), `ENODEV` (hypervisor not armed), `EBUSY` (a VM exists), `ENOMEM` (no task slot) |
| stop | `write` `stop` |
| status | `write` `status`: one `[kvm] status: state= gen= owner= up_s= type= vcpus= ram_mib= forwards= reserve_mib= console_lost=` line is appended to the console stream |
| console | `read`: bytes; `EAGAIN` while the guest lives and is quiet; `0` once it has ended and the stream is drained. One reader: the cursor belongs to the device |

Why a character device: it takes effects (a `/proc` file would be read-only plumbing) and a
Firecracker-shaped `/dev/kvm` can sit beside it later; why not a syscall: nothing else in this kernel
adds one for a single driver. It is opened by the kernel's own `openat` arm like `/dev/power`, so
`ls /dev` does not list it; `open` is `ENOENT` unless the kernel was booted with `kvmctl`.

- **The VM's lifetime is its owner's.** The creating process is the lease-holder; the run loop checks
  every 200 ms that it is still alive and stops the guest when it is not. That is what lets herd
  supervise a VM with a pid alone: `herd stop`, a crash or a `kill` of the `vm-run` process ends the
  guest, and the ordinary `restart`/`max_retries` policy brings it back (a replacement `create` waits
  out `EBUSY` while the old one tears down). A boot-flag guest has no owner and no lease.
- **One VM at a time; one VM task.** The task is spawned on the first `create`, pinned to a core with
  SVM enabled, and serves every later VM. Every virtual processor of the VM takes turns inside it.
- **The console goes to a ring, not the host's.** While a `/dev/kvm0` VM runs, everything the
  hypervisor prints for it (the guest's serial and its own `[KVM]` lines) goes to a 32 KiB static
  ring (`akuma_dmesg::Ring`) instead of the host's serial line and dmesg. If the reader is slower than
  the guest the oldest bytes are lost (`console_lost`). The 100 ms drain keeps that rare.
- **No allocation on any of it** apart from the RAM-disk copy's 1 MiB chunk (fallible, freed). The spec
  is a `Copy` value; the wire document is capped at 768 bytes and read onto the stack.
- **A spec that names a disk does not start without it.** A refused RAM copy or a missing image ends the run
  at once (`refused: the spec's disk is not available`) instead of booting a rootless guest.
- **Reserved memory (`kvmmem=<MiB>`).** The kernel takes one contiguous block at boot, while memory is
  unfragmented, and every VM's RAM and RAM-disk copy is a 2 MiB-aligned slice of it
  (`akuma_vm_spec::arena`, first fit, zeroed when carved). **A request that does not fit is refused**
  (`[KVM] linux: refused: guest RAM does not fit the kvmmem= reserve`), never retried from the page
  allocator; without `kvmmem=` the page allocator is used, as it always was. Guest RAM is bounded at
  1 GiB by the nested page table (one directory of 2 MiB leaves) and the Akuma guest needs ≥ 768 MiB.
- **Cores (`kvmcores=<list>`, e.g. `4-7`).** The VM task is placed on the first listed core that has
  SVM enabled; default core 0. This moves the *whole* hypervisor off the boot core. It is **not yet a
  reservation** — nothing keeps other tasks off that core — and it does not spread vCPUs. Real core
  reservation is the multi-vCPU step: one pinned task per vCPU (own VMCB and run loop), the guest's RAM,
  NPT, device models, gateway and relay behind locks or handed to one owner core, a placement policy
  (which cores a VM may use, so host services keep some), and cross-core delivery of virtual IPIs
  (today an IPI is a flag set in the same loop). Until then more vCPUs add context switches, not compute.

| herd key | |
|---|---|
| `vm = true` | the service is a VM (`command` is not needed; it is `herd vm-run`) |
| `vm_type` | `kvm` (default) or `firecracker` (reserved: parses, the service is not loaded) |
| `vm_kernel`, `vm_cmdline` | kernel image (an Akuma ELF), the guest command line tail (rest of the line) |
| `vm_ram_mib`, `vm_vcpus` | 16–1024 (default 768), 1–4 (default 1) |
| `vm_disk`, `vm_disk_ram` | image path, or `/dev/nvme0n1p<N>` (that partition in place; never with `vm_disk_ram`); empty: the machine's second virtio-blk. `vm_disk_ram` copies an image into RAM at start |
| `vm_forward` | `host:guest`, repeatable up to four |
| `vm_config` | a Firecracker JSON config (`boot-source`, `machine-config`, `drives[0]`) read first; explicit `vm_*` keys win |

The spec keys use Firecracker's words as aliases (`kernel_image_path`, `boot_args`, `vcpu_count`,
`mem_size_mib`, `path_on_host`), so a config written for one backend needs no translation for the other;
network interfaces are not imported (a guest reaches the host through the built-in gateway; ports are
forwarded). **No Firecracker or Linux/KVM source is imported or read** — only the config schema's field names
are used; its crates' `kvm-bindings` come from the Linux UAPI headers and stay out.

Kernel parameters: `kvmctl` (arm SVM for `/dev/kvm0`, run nothing at boot), `kvmmem=<MiB>`,
`kvmcores=<list>`. The older `kvmtest` + `kvmserve`/`kvmgentoo`/selector-file path is a shim: it builds the
same `Spec` (`kvm::legacy_service`) and queues it with no owner, so the rehearsal flags and entry 28 keep working.

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
