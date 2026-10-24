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
  **The arm functions (`arm_deadline`, `arm_oneshot_in_us`, `arm_idle_shot`) run with interrupts
  off, whatever the caller's `IF`**: a tick between the count write and the `ONESHOT` store is
  not restored, and the `sti; hlt` after it sleeps with no timer for good
  (`archive/FIRECRACKER_GUEST_HANG.md`). The ring dump `[kvmapi] trace:` prints when a guest halts with
  nothing due and nothing that can wake it.
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
- **The NIC is polled and has no route out unless the spec says `vm_outbound`** (see *Outbound networking* below). `Guest::AkumaNet` puts the virtio-net window
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
- **Cores (`kvmcores=<list>`, spec `vm_cores`).** `kvmcores` is the machine's allowed set (default core 0), `vm_cores`
  the VM's own mask (default: everything allowed); `akuma_vm_spec::cores::place` puts virtual processor `i` on the
  `i`-th usable core of the intersection (wrapping: processors sharing a core take turns, as before placement). The
  VM task repins itself to processor 0's core; every other core gets a persistent worker task
  (`kvm/linux/machine.rs`) that serves the processors placed there. While a service VM runs, its cores (never core 0)
  are **reserved**: the picker hands them only tasks pinned to them (`sched::reserve_cores`). With `kvmcores=` given, the whole set is reserved **from boot** (less core 0), so the host schedules nothing there even with no VM running; a core is **not even started** by the host when `smp=` cuts it off: `smp::ensure_started` wakes it (and every deferred core below it, so the online set stays contiguous) the first time a VM is placed on it, with the same AP path and TSC-based delays; until then the firmware's wait-for-startup state is all it has seen (`smp=6 kvmcores=4-7 vm_cores=4,7` on the ryzen starts cores 6 and 7 only when the VM does). A started core stays up and reserved; there is no unplug. Shared state is guest RAM
  and the NPT (read-only once built), the devices behind one lock, and a state word + `poke` flag per processor; a
  cross-core IPI, INIT, STARTUP, or a disk interrupt for processor 0 sets the flag and sends the wake IPI
  (`smp::kick_core`). A service guest's exits are handled **without the BKL** (the whole of `serve` is one dropped-BKL window), so groups
  contend on the device lock alone; the BKL is taken only for a host-backed disk (`HostDisk::bkl`), the lease check's
  foreign-pid lookup and `yield_now`'s switch, always *after* the device lock, never before it. Rehearsal guests keep the
  old BKL-held shape. Never hold the device lock across `yield_now`, `VMRUN` or a sleep. **Pick cores that are not SMT siblings**: on the ryzen 2,3 gave two guest
  loops 63% each, 2,4 gave 98% (`docs/archive/KVM_CPU_ACCOUNTING.md`). A `[KVM] cpu:` line per task shows where
  each core's time goes.

```
 machine: kvmcores=1-7 (allowed)      VM spec: vcpus=2  vm_cores=2,4
                                       place() = allowed & vm_cores -> vCPU0 on core 2, vCPU1 on core 4

 core 0          core 1        core 2 (reserved)      core 3        core 4 (reserved)     core 5..7
 ------------    ----------    -------------------    ----------    -------------------   ----------
 boot thread     sshd          vm_task (coordinator)  netpoll       worker task           herd, wifi,
 pinned here,    netpoll       = serve(vCPU0, owner)  user procs    = serve(vCPU1)        user procs
 never reserved  user procs    relay, NIC rx          run here      guest code only       run here
 user procs      run here      guest code
 run here
                               RESERVED_CORES = {2,4}: sched::hook_can_run() answers "no" for any
                               thread that is not pinned to that core -> sshd & co. never land there

 vCPU0 <--- state word + poke flag + wake IPI (smp::kick_core) ---> vCPU1
        INIT / STARTUP / fixed IPI / disk interrupt for the PIC (vCPU0's)

 shared by both tasks:  guest RAM + NPT (read-only after build)
                        Spinlock<Devices>: PIT, PIC, UART, LAPICs, virtio-blk, virtio-net+gateway, relay
```

Reservation is a mask the picker consults: a reserved core only runs threads pinned to it. It is set
when the VM starts (cores of all vCPUs, minus core 0) and cleared when it ends; a thread already on the
core keeps it until its next switch-out, after which it can only be placed elsewhere.

| herd key | |
|---|---|
| `vm = true` | the service is a VM (`command` is not needed; it is `herd vm-run`) |
| `vm_type` | `kvm` (default) or `firecracker` (reserved: parses, the service is not loaded; a Firecracker VM runs as an ordinary `command =` service against `/dev/kvm`, see above) |
| `vm_kernel`, `vm_cmdline` | kernel image (an Akuma ELF), the guest command line tail (rest of the line) |
| `vm_cores` | host cores for the vCPUs (`2,4`), within `kvmcores=` |
| `vm_ram_mib`, `vm_vcpus` | 16–1024 (default 768), 1–4 (default 1) |
| `vm_disk`, `vm_disk_ram` | image path, or `/dev/nvme0n1p<N>` (that partition in place; never with `vm_disk_ram`); empty: the machine's second virtio-blk. `vm_disk_ram` copies an image into RAM at start |
| `vm_forward` | `host:guest`, repeatable up to four |
| `vm_outbound` | `true`: the guest's TCP connections and DNS queries leave through the host (default off: no route out) |
| `vm_config` | a Firecracker JSON config (`boot-source`, `machine-config`, `drives[0]`) read first; explicit `vm_*` keys win |

The spec keys use Firecracker's words as aliases (`kernel_image_path`, `boot_args`, `vcpu_count`,
`mem_size_mib`, `path_on_host`), so a config written for one backend needs no translation for the other;
network interfaces are not imported (a guest reaches the host through the built-in gateway; ports are
forwarded). **No Firecracker or Linux/KVM source is imported or read** — only the config schema's field names
are used; its crates' `kvm-bindings` come from the Linux UAPI headers and stay out.

Kernel parameters: `kvmctl` (arm SVM for `/dev/kvm0`, run nothing at boot), `kvmmem=<MiB>`,
`kvmcores=<list>`. The older `kvmtest` + `kvmserve`/`kvmgentoo`/selector-file path is a shim: it builds the
same `Spec` (`kvm::legacy_service`) and queues it with no owner, so the rehearsal flags and entry 28 keep working.

## Outbound networking (`vm_outbound`, 2026-10-23)

Off by default: the gateway is "a model, not a route" and a guest then reaches nothing but its gateway. With
`vm_outbound = true` in the herd conf (kernel side: `Spec::outbound`, the wire key `outbound=1`; a service guest only,
the legacy boot flags never get it) the gateway lets two things out and nothing else:

* **TCP to an address outside the guest's `10.0.2.0/24`.** The gateway *holds* the guest's SYN in one of
  `OUT_SLOTS` (8) outbound slots and tells the relay (`Gateway::out_take_request`). The relay makes the same
  connection from the host's stack (a non-blocking `socket_connect`) and answers the guest with
  `out_accept` (a SYN-ACK *from the real address and port*, so the guest believes it reached it) or
  `out_refuse` (a RST). After that the bytes move through the same `shuttle` as a forward slot. Not
  forwarded: the guest's own subnet, `0/8`, `127/8`, multicast and broadcast (`gateway::routable`,
  `relay::outbound_allowed`); a ninth connection is reset (`out_full`).
* **DNS to the proxy `10.0.2.3:53`.** The query is queued (`DNS_SLOTS` = 4, up to `DNS_MAX` = 512 bytes), sent
  verbatim by the relay over one UDP socket to `akuma_net::dns::upstream()` (the host's own first resolver),
  and the answer is matched back by id and sent to the guest from `10.0.2.3`. A query without an answer for 5 s is
  dropped (the guest retries).

What it does **not** do: ICMP past the gateway (`ping 8.8.8.8` fails), UDP other than DNS, IPv6, inbound beyond the
`vm_forward` list. **Isolation changes with it:** a guest's connections leave from the *host's* address, so a
guest with `vm_outbound` reaches whatever the host reaches (its LAN, its own LAN address and the host's sshd
included; loopback excepted). Turn it on for guests you trust with the host's network.

The relay's per-slot state is heap, allocated once on the first outbound poll (the relay is built by value on a
small task stack). `[KVM] relay: outbound opened= failed= denied= ... gw_syns= gw_full= gw_refused=` and one
`out slot` line per live connection are in the status output. Rehearse it with
`scripts/kvm/serve_rehearse.py LOG --herd --outbound` (a web server on the Mac by LAN address; DNS; the Alpine CDN).
How it was built, verified, and the one open item: [`KVM_OUTBOUND_NETWORKING.md`](../../archive/KVM_OUTBOUND_NETWORKING.md).

## `/dev/kvm`: the KVM-ioctl node (an unmodified Firecracker, 2026-10-23)

Beside `/dev/kvm0` (a spec written, the kernel runs the whole VM) there is **`/dev/kvm`**, the Linux-shaped protocol a
userspace VMM speaks: system fd -> `CREATE_VM` -> VM fd -> `CREATE_VCPU` -> vCPU fd, `KVM_RUN` as the vCPU loop, the VMM
emulating every device but the PIT, PIC and local APIC. **Grade C.** Armed by `kvmctl` (or `kvmapi`); `open` is `ENOENT`
otherwise. Verified: Alpine's `firecracker --no-api --no-seccomp --config-file vm.json` boots the Akuma kernel as a guest
(Firecracker-served virtio-blk root, serial through its emulated 16550) on TCG and on the ryzen
([`../../runbooks/run-firecracker-on-akuma.md`](../../runbooks/run-firecracker-on-akuma.md)). How the protocol was learned and
what is unconfirmed: [`../../archive/AKUMA_FIRECRACKER_KVM.md`](../../archive/AKUMA_FIRECRACKER_KVM.md). Code: the pure crate
`crates/akuma-kvm-abi` (request decode, vCPU argument layouts, the memslot table, the run page; `forbid(unsafe)`),
`amd64/src/kvm/api.rs` + `api/{vcpu,platform}.rs`, `mm::{remap_user_to_phys, mmap_frames}`.

- **Handles.** `FileDescriptor::DevKvm(id)`: 0 is `/dev/kvm0`, `n > 0` names a slot of the handle table (system / VM / vCPU),
  so `dup` needs no reference counts. A handle, and the VM, live until their owner process is gone (reaped by the next
  `open`, `CREATE_VM`, or a `/dev/kvm0` create). **One VM, and not beside a `/dev/kvm0` VM**: either side is `EBUSY`.
- **Guest RAM is a slice of the `kvmmem=` reserve, remapped into the VMM.** `SET_USER_MEMORY_REGION` carves a 2 MiB-aligned
  slice (zeroed), copies the VMM's range into it and moves the VMM's PTEs onto it; the leaves are *borrowed* (not in the
  frame ledger, no share count: `munmap`/exit/`fork` never free them) and the NPT is the same 2 MiB-leaf table `kvm0` builds.
  No `kvmmem=` or no room: `ENOMEM`. Guest RAM <= 1 GiB, regions 2 MiB aligned, a slot is set once, no dirty logging.
  **A VMM that forks after `CREATE_VM` is logged** (`[kvmapi] WARNING`), not prevented: the child would CoW-share frames
  the guest writes through.
- **`KVM_RUN` is the VMM thread's syscall.** The thread is pinned for the run to its vCPU's core (`sched::repin_current`; SVM enabled on
  demand), the host FPU/SSE state is saved around every `VMRUN`, the BKL is released **only** around `VMRUN` (the exit path
  and the eventfd wake need it held), and `yield_now` runs between entries because the kernel switches cooperatively (a
  woken device thread otherwise never runs on a one-core host). Signals end it with `EINTR`.
- **Placement is the vCPU thread's affinity** (`taskset -c 4-7 firecracker ...`, or `cpus = 4-7` on a herd service; both are
  `sched_setaffinity`, inherited by the threads the VMM creates). vCPU `i` runs on the `i`-th core of that mask intersected with
  `kvmcores=` (`akuma_vm_spec::cores::place`); with no mask and no `kvmcores=` the thread stays where it is, with no mask and
  `kvmcores=` the vCPUs spread over the reserved cores. `[KVM] vcpu N -> core C` is logged for the first runs. A reserved core
  runs only threads pinned to it or whose affinity names it. Design: `../../archive/AKUMA_FIRECRACKER_KVM.md` § *CPU affinity*.
- **Exits.** CPUID (from the VMM's own table), MSRs, PIT/PIC ports and the LAPIC page are the kernel's; every other port and
  MMIO address is returned through the run page and completed at the next `RUN`. A registered `IOEVENTFD` doorbell signals its
  eventfd and never leaves the kernel; an `IRQFD` eventfd is **polled** at every entry and idle wake and raises PIC pin `gsi`
  (< 16): up to a millisecond of interrupt latency, no waker hook yet.
- **Never put a large value on the kernel stack in this path.** The kernel stack is **32 KiB with no guard page** (heap-allocated,
  `sched::STACK_SIZE`); an overflow corrupts the neighbouring heap chunk. The vCPU, its CPUID table, FPU image and event
  snapshot are heap-allocated once at `CREATE_VCPU` (fallible) and `RUN` moves pointers; the first version copied ~6 KiB
  through closures and the ryzen died in `talc::free` on the fifth run (`../../archive/AKUMA_FIRECRACKER_KVM.md` § *Metal*).
- The run-page layout, the `IOEVENTFD` flag bits and the `SET_FPU` layout are **hypotheses confirmed only as far as
  Firecracker exercised them**; `GET/SET_LAPIC`, the I/O APIC, `SET_GSI_ROUTING` and more than one vCPU are not done.

## Open items (2026-10-23)

* **Firecracker on `/dev/kvm`: the to-do list that ends the KVM work** is in [`../../archive/AKUMA_FIRECRACKER_KVM.md`](../../archive/AKUMA_FIRECRACKER_KVM.md) § *Before the KVM work is finished*: CPU **affinity** (placing vCPU threads and `sched_setaffinity`), the I/O APIC, `GET/SET_LAPIC`, more than one vCPU, seccomp, a kernel-stack guard, a real `HLT` block, an irqfd waker, NetBSD, and lifting the one-VM exclusivity.

* **Large guest->host transfers wedge the guest** (96 MiB+ through the relay), on the old kernel too; the host's
  `/bin/ssh` client spins while the relay's host socket stays full. Evidence and next experiments:
  [`KVM_BKL_EXIT_HANDLING_AND_BULK_WEDGE.md`](../../archive/KVM_BKL_EXIT_HANDLING_AND_BULK_WEDGE.md).
* **Outbound to `neverssl.com:80`** is refused although the Mac reaches the same address
  ([`KVM_OUTBOUND_NETWORKING.md`](../../archive/KVM_OUTBOUND_NETWORKING.md) § Open).
* **NetBSD as a guest**: `netbsd-MICROVM` boots unmodified on the metal (PVH, `console=com`, virtio-mmio NIC) and stops at
  its root prompt; it needs a root disk. One VM at a time through `/dev/kvm0` (a second service gets errno 16).
  [`AKUMA_KVM_NETBSD_GUEST.md`](../../archive/AKUMA_KVM_NETBSD_GUEST.md).

## Isolation modes under herd (one supervisor, four shapes of "a machine")

```
                                  herd  (init, pid 1: reads /etc/herd/enabled/*.conf)
                                    |   same restart / max_retries / log policy for every shape
        +---------------------------+-----------------------------+------------------------------+
        |                           |                             |                              |
   (a) process                (b) box                      (c) VM, Akuma kernel           (d) VM, Linux kernel
   command = ...              boxed = true                 vm = true                      vm = true
   no isolation               box_root / bundle            vm_kernel = /boot/akuma-guest  vm_kernel = vmlinux
                              (OCI rootfs, overlay)        vm_type = kvm                  (PVH; legacy boot flag
        |                           |                             |                         only today, not yet
        |                  +--------+---------+          +--------+---------+               a herd-run guest)
        |                  |                  |          |                  |                     |
        |             box: Alpine        box: Gentoo   guest: Akuma     guest: Akuma          guest: Linux
        |             (OCI image,        (/opt/gentoo  kernel +         kernel + Gentoo       kernel + any
        |              apk userland)      root on p3)  busybox root     userland on p7        root (virtio-blk)
        |                  |                  |          |                  |                     |
 isolation by:       namespaces in ONE Akuma kernel      hardware: SVM + nested paging, own kernel,
                     (mount/pid/net views, fs policy)    own RAM slice of the kvmmem= reserve, own cores
        |                  |                  |          |                  |                     |
 network:            host stack (smoltcp) or a rump     built-in gateway 10.0.2.2 (DHCP/ARP/ICMP), host
                     box via stack = rump; ports are    ports forwarded in: vm_forward = host:guest
                     the host's                         (a relay listens on the host, one per forward)
        |                  |                  |          |                  |                     |
 storage:            host fs         overlay on OCI     vm_disk = image file, RAM copy (vm_disk_ram), or a
                                     layers / SubdirFs  GPT partition in place (/dev/nvme0n1p7); the host's
                                                        own root is never offered
        |                  |                  |          |                  |                     |
 config:             command, args   bundle = OCI       vm_ram_mib, vm_vcpus, vm_cores, vm_cmdline,
                                     config.json        or vm_config = a Firecracker JSON (boot-source,
                                                        machine-config, drives[0]); vm_* keys win
```

Everything above is a herd service: `herd start/stop/status`, restart policy and `/var/log/herd/<name>.log`
are the same. A VM's lifetime is its supervising process (`herd vm-run`); the guest's console is its log.
Boxes share one kernel and cost almost nothing; VMs cost a RAM slice and cores (reserved from the host,
`kvmcores=`/`vm_cores`) and give a separate kernel. Firecracker itself runs as an ordinary process against `/dev/kvm` (above); `vm_type = firecracker` stays reserved.

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
