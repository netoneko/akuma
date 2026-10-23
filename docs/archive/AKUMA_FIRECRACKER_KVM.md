# Running the unmodified `firecracker` on Akuma's SVM hypervisor (2026-10-23 ->)

Goal: `firecracker --no-api --config-file vm.json` (Alpine's apk binary) boots a guest, first
`/boot/akuma-guest`, then NetBSD's `netbsd-MICROVM`, with Akuma providing the KVM ioctl ABI.
Branch `kvm-part-1`. Clean room: no Linux/KVM/Firecracker source or UAPI headers are read; the ABI is
learned by observing the binary. Progress log; phases are stopped-and-reported in order.

## Phase 0: can the binary run at all? — YES (2026-10-23)

Binary: Alpine `community/x86_64/firecracker-1.16.0-r0.apk` (`usr/bin/firecracker`, 2.7 MB, stripped,
dynamic PIE, interpreter `/lib/ld-musl-x86_64.so.1`; needs `libc.musl` and `libgcc_s.so.1` from the
`libgcc` apk). Staged by `scripts/kvm/firecracker_phase0.sh` (ext2 root with busybox, the vendored
musl, libgcc, firecracker, a `vm.json`) and booted on the Mac under QEMU microvm/TCG with `strace_err`.

| step | result |
|---|---|
| `firecracker --version` | `Firecracker v1.16.0`, exit 0. No missing syscall. Only ENOENT probes: `/etc/ld-musl-x86_64.path`, `/etc/localtime` |
| `firecracker --no-api --config-file /vm.json` | parses the config, logs `Running Firecracker v1.16.0`, then `[sc!] nr=2 ... -> -2 "/dev/kvm"`; exits 1: `Error creating KVM object: No such file or directory` |

So the distance to the first real wall is zero syscalls: the loader, threads, the logger and config parsing
all work. The first (and so far only) failure is the expected `open("/dev/kvm")`. Everything after it is
the ioctl surface (Phase 1).

Note for Phase 1: firecracker's seccomp filters and `/dev/kvm` ACL hint are only reached later; with
`--no-api` and no jailer neither cgroups nor chroot are needed.

## Phase 1: census (2026-10-23)

Method: `kvmapi` boot flag (with `kvmctl`, which is what makes `kvm::init` run) arms `amd64/src/kvm/api.rs`:
`open("/dev/kvm")` hands out a descriptor and every `ioctl` on it is logged (fd, request decoded to
dir/size/type/nr, argument, the argument struct before and after) and answered from a tiny table. Everything
unknown answers 0. `mmap` of such a descriptor is served as anonymous memory and logged. Driver:
`scripts/kvm/fc_census.sh` (rebuild, boot, print the `[kvmapi]` lines), over `firecracker_phase0.sh`.
`firecracker --no-api --no-seccomp --config-file vm.json` with one drive, 1 vCPU, 128 MiB.

**How the ABI was learned.** Request codes are `dir(2) size(14) type(8) nr(8)`; every request is type `0xAE`.
Names below are *inferred* from direction, size, argument contents and position in the boot sequence, not
read from anywhere. Two values were *probed*: `nr 0` (API version) was answered `12` on the first try (a
number recalled from general knowledge of the public interface, accepted by the binary; `-1` gives
"invalid KVM API version"), and `nr 4` (vCPU mmap size) `0x2000` was accepted. `CHECK_EXTENSION` answered
ENOTTY for every capability lets the walk reach `GET_SUPPORTED_CPUID`; answered `0` for the first one it
fails ("Missing KVM capabilities: 0x0"); answered `1` for all it proceeds. Which capabilities are actually
required is not yet separated (open, Phase 2).

### Ioctls, in the order the binary issues them (all on `/dev/kvm` unless noted)

| # | request | inferred name | argument as observed |
|---|---|---|---|
| 0 | `0xAE00` | GET_API_VERSION | none; must return the expected version (12 accepted) |
| 1-14 | `0xAE03` | CHECK_EXTENSION | capability id: 0x00, 0x24, 0x20, 3, 4, 0x21, 0x23, 0x27, 0x32, 0x0e, 0x29, 0x38, 0x37, 7; later 0x0a (system fd) and 0xd0 (VM fd) |
| 15 | `0xC008AE05` | GET_SUPPORTED_CPUID | `{u32 nent, u32 pad}` then `nent` entries; Firecracker offers `nent=0x100`; `E2BIG` path not exercised |
| 16 | `0xAE01` | CREATE_VM | arg 0; returns the **VM fd** |
| 17 | `0xAE04` | GET_VCPU_MMAP_SIZE | returns 0x2000 (8 KiB: the run page plus room) |
| 18 | `0xAE03` | CHECK_EXTENSION 0x0a | (on system fd) |
| 19 | `0xC004AE02` | GET_MSR_INDEX_LIST | `{u32 nmsrs}`=0x10000 capacity; answered 0 entries, accepted |
| VM fd | `0xAE03` 0xd0 | CHECK_EXTENSION | |
| | `0xAE47` | SET_TSS_ADDR | arg = `0xfffbd000` (value, not pointer) |
| | `0xAE60` | CREATE_IRQCHIP | none: **Firecracker asks for the in-kernel PIC/IOAPIC** |
| | `0x4040AE77` | CREATE_PIT2 | 64 bytes, first word `1` (flags): **in-kernel PIT** |
| | `0xAE41` | CREATE_VCPU | arg = vcpu id (0); returns the **vCPU fd**; the fd is then `mmap`ed shared, 0x2000 bytes, offset 0 (the run page) |
| | `0x4020AE46` | SET_USER_MEMORY_REGION | 32 bytes `{slot=0, flags=0, gpa=0, size=0x08000000, uaddr=0x1_0003_c000}`: **guest RAM is the VMM's own anonymous mmap, 4 KiB aligned, not 2 MiB** |
| | `0x4020AE76` x4 (x5 with a drive) | IRQFD | 32 bytes `{u32 eventfd, u32 gsi, flags...}`; gsi 4, 1, 5, 6 (the serial line and the rest) |
| | `0x4040AE79` (only with a drive) | IOEVENTFD | 64 bytes; one per virtio queue doorbell |
| vCPU fd | `0x4008AE90` | SET_CPUID2 | header + 61 entries; **entry stride 40 bytes confirmed** (entry 0 = leaf 0 with `AuthcAMDenti`, entry 1 starts 40 bytes on) |
| | `0x4008AE89` | SET_MSRS | 11 entries of `{u32 index, u32 pad, u64 data}`: 0x10, 0x174-0x176, 0x1a0, 0x2ff, ...; must return the count set, else "SetMsrsIncomplete" |
| | `0x4090AE82` | SET_REGS | 144 bytes |
| | `0x41A0AE8D` | SET_FPU | 416 bytes |
| | `0x8138AE83` / `0x4138AE84` | GET_SREGS / SET_SREGS | 312 bytes each |
| | `0x8400AE8E` / `0x4400AE8F` | GET_LAPIC / SET_LAPIC | 1024 bytes: **in-kernel local APIC state** |
| | `0xAE80` | RUN | none; the run page's exit reason decides what happens next |

At `RUN` the stub returns 0 with an all-zero run page; Firecracker logs `MicroVMStopped without an error:
GenericError` and exits 1. That is the end of what a stub can show: runtime ioctls (interrupt injection,
`GET/SET_VCPU_EVENTS`, `KVM_IRQ_LINE`, dirty-log, `SET_CLOCK`/`GET_CLOCK`, `KVM_SIGNAL_MSI` ...) only appear
once exits are delivered. Their discovery belongs to the Phase 3 slice (answer a scripted `KVM_EXIT_IO`
and watch).

### What the binary needs from Akuma besides ioctls

Every syscall Firecracker made succeeded; the numbers it used (x86_64): read write open close stat fstat poll
lseek mmap mprotect munmap brk rt_sigaction rt_sigprocmask ioctl madvise dup dup2 getpid clone fork execve
exit wait4 uname fcntl getcwd getuid getppid sigaltstack prctl arch_prctl gettid **tkill** futex
set_tid_address clock_gettime exit_group **epoll_ctl epoll_pwait epoll_create1 timerfd_create timerfd_settime
eventfd2** prlimit64 getrandom **membarrier**. Only failures: `arch_prctl(0x1021)` -> EINVAL (harmless),
`stat("/dev/sysgenid")` -> ENOENT (harmless, absent on most hosts), and **`seccomp(SECCOMP_SET_MODE_FILTER)`
(nr 317) -> ENOSYS, which panics the vCPU thread** ("Failed to set the requested seccomp filters on vCPU 0").
`--no-seccomp` avoids it. Decision for Phase 2: either accept-and-ignore a seccomp filter (a lie about
isolation, but the VMM is the only user and runs as root anyway) or keep requiring `--no-seccomp`.
No jailer, cgroups or userfaultfd were touched. `tkill` is the vCPU-kick signal (SIGRTMIN-class) the VMM
uses to interrupt a `KVM_RUN`; `KVM_RUN` must therefore return `EINTR` when a signal arrives.

### Phase 1 verdict

The shape of the problem is now concrete: **Firecracker wants an in-kernel PIC+IOAPIC (`CREATE_IRQCHIP`), PIT
(`CREATE_PIT2`) and LAPIC (`GET/SET_LAPIC`), guest RAM from its own anonymous memory (4 KiB granularity), and
a per-vCPU run page with `KVM_RUN` exits for port I/O and MMIO**, while Akuma's hypervisor has no IOAPIC, a
PIC/PIT/LAPIC that are fully in-kernel, RAM from a reserved 2 MiB-aligned block, and a one-VM node. Stub
and census scripts: `amd64/src/kvm/api.rs`, `scripts/kvm/fc_census.sh`.

Added after the first census: `SET_REGS`/`SET_SREGS` show a **PVH** entry (`rip=0x201000`, `rbx=0x6000`,
`CR0=0x11`, flat 32-bit segments, GDT at 0x500), i.e. the same entry state the hypervisor already builds.

## Phase 2: design (2026-10-23)

Design (moved here from `proposals/KVM_ABI_SHIM.md`, 2026-10-23). Nothing below is built except the logging stub `amd64/src/kvm/api.rs`; evidence for what Firecracker asks for is the Phase 1 census above. Names are inferred from request direction/size/behaviour.

Status: **design for approval** (phase 2 of `docs/archive/AKUMA_FIRECRACKER_KVM.md`; nothing here is built
except the logging stub `amd64/src/kvm/api.rs`). Evidence for every statement about what Firecracker
asks for is the phase-1 census in that archive doc. Clean room: nothing below comes from Linux/KVM or
Firecracker source or UAPI headers; names are inferred from request direction/size/behaviour.

### 1. What the census says Akuma must provide

Firecracker 1.16 on x86_64, `--no-api`, one drive, one vCPU, boots a **PVH** ELF kernel: at the first
`KVM_RUN` it has set `rip=0x201000` (the ELF's PVH entry), `rbx=0x6000` (`hvm_start_info` it wrote in guest
RAM), 32-bit flat protected mode (`CR0=0x11`, no paging, flat code/data selectors 0x8/0x10, a GDT at
0x500), exactly the entry state `amd64/src/kvm/linux.rs` already builds for its own guests. It asks for:

| need | what it is | Akuma has |
|---|---|---|
| 3 fd kinds | system fd -> VM fd (`CREATE_VM`) -> vCPU fd (`CREATE_VCPU`), vCPU fd `mmap`ed (run page, 8 KiB) | one-VM control node `/dev/kvm0`, no per-VM fds |
| guest RAM = VMM memory | `SET_USER_MEMORY_REGION` {gpa, size, **uaddr in the VMM's anonymous mmap**, 4 KiB aligned} | RAM = one 2 MiB-aligned block of a boot-time reserve, 2 MiB NPT leaves, <= 1 GiB |
| `KVM_RUN` as the vCPU loop | exits to userspace for port I/O and MMIO; `EINTR` on signal (`tkill` is its kick) | loop is a kernel task per core; every device in-kernel |
| in-kernel irqchip | `CREATE_IRQCHIP` (PIC+IOAPIC), `CREATE_PIT2`, `GET/SET_LAPIC` | PIC, PIT, LAPIC models; **no IOAPIC** |
| interrupt plumbing | `IRQFD` (eventfd -> gsi: serial, virtio), `IOEVENTFD` (MMIO doorbell -> eventfd) | none; virtio is in-kernel |
| vCPU state | `SET_CPUID2` (61 entries, 40-byte stride), `SET_MSRS`, `SET_REGS`, `SET_FPU`, `GET/SET_SREGS`, `GET/SET_LAPIC` | VMCB layouts, `cpuid::filter`, MSR store; no get/set marshalling |
| housekeeping | `GET_API_VERSION`, `CHECK_EXTENSION` x16, `GET_SUPPORTED_CPUID`, `GET_MSR_INDEX_LIST`, `SET_TSS_ADDR` | cpuid filter |
| host syscalls | eventfd2, epoll, timerfd, tkill, membarrier, ... all already work | yes. `seccomp(2)` -> ENOSYS (panic unless `--no-seccomp`) |

### 2. The hard parts, sized

**(a) Guest memory is the VMM's own memory: DECIDED (owner, 2026-10-23) - reserved slice, remapped.**
Firecracker `mmap`s anonymous memory and passes it as `uaddr`. Rather than pin scattered frames behind a
4 KiB NPT, the shim **gates Firecracker to the `kvmmem=` reserve**: at `SET_USER_MEMORY_REGION` it takes a
2 MiB-aligned slice of the arena (`akuma_vm_spec::arena`, the kvm0 allocator), copies the range's current
contents into it, and rewrites the VMM's PTEs for `[uaddr, uaddr+size)` onto the slice frames ("direct
memory transfer"). The existing `build_npt` (2 MiB leaves, <= 1 GiB) then maps the guest unchanged, and the
VMM and the guest share the same frames with no refcounts, no pinning and no on-demand faults. Rules: the
frames are **borrowed** (a mapping kind that never enters the process's frame ledger or the PMM free path:
`munmap`/exit only drop PTEs); the slice returns to the arena when the VM fd closes (like kvm0's RAM and
`DiskLease`); a request that does not fit the reserve is refused (no `kvmmem=` -> `ENOMEM`). A VMM that
forks after `CREATE_VM` is logged to dmesg, not prevented. Rejected alternative, kept for the record: pin
the VMM's scattered frames behind a 4 KiB-leaf NPT built on demand (needs a write fault per page so no CoW
share remains, a 4 KiB NPT builder, and "no munmap/fork" rules; more general, more code, no reserve needed).
Cost: a PTE-rewrite + borrowed-frames operation in `akuma-mmu`/`akuma-user-space` (to be written; first
task of slice 1), ~200 lines + tests.

**(b) `KVM_RUN` in syscall context.** The ioctl runs the world-switch loop on the calling thread, like the
real thing: no VM task, the VMM's own threads are the vCPUs. This removes the cross-address-space problem in
(a) and the lease/owner machinery (the VM dies with its fds). Requirements: the thread must be on an SVM-enabled
core for the duration (pin it; `SVM_ENABLED` is per core and only cores in `kvmcores=`/core 0 have it today:
extend `enable_on_core` to run on demand for the chosen core); `VMSAVE/VMLOAD` the host state each entry
(other tasks run in between); host IRQs enabled, `#VMEXIT` on the physical interrupt, yield and signal check at
each exit (`EINTR` -> the run page's "interrupted" reason). Exit handling splits by the `kvm0` rule "a model in
the kernel, everything else outside": CPUID/MSR/HLT/PAUSE/PIT/PIC/LAPIC/IOAPIC handled in-kernel (reusing the
pure models and `mmio::decode`); other port I/O and other MMIO are **returned to userspace** in the run page
and *completed on the next `KVM_RUN`* (a pending-completion state: advance `RIP`, latch the data of an `IN`/
load into the register). The run page layout (exit reason, io/mmio sub-structs) is not visible in the census
(only the VMM reads it); it is learned in the first Phase 3 slice by scripting one port-I/O exit at a
hypothesised layout and watching Firecracker's behaviour, with the layout then pinned in the pure crate.
Cost: a new ~500-line loop (`run_vcpu`) plus ~300 lines of completion state. I recommend **not** refactoring
`serve` in `machine.rs` (1.4k lines, entangled with the relay, the gateway and the multi-core placement of
the `kvm0` model): reuse the *pure* pieces and `init_vmcb`, leave `kvm0` byte-for-byte alone.

**(c) ioeventfd / irqfd.** ioeventfd: on an MMIO write exit that matches a registered (gpa, len, datamatch),
signal the eventfd in the kernel and *do not* exit to userspace (doorbells are the hot path). irqfd: a kernel
watcher on the eventfd that raises the gsi on the irqchip and wakes a halted vCPU. `eventfd` lives in
`akuma-syscalls-glue` (shared with the AArch64 kernel), so both need a small notifier hook (callback on write,
and a non-blocking `signal(fd)`/`drain(fd)` pair): ~150 lines + the hook in glue. A poll-only irqfd (check each
registered eventfd at every exit and idle tick) works first with <= 1 tick of interrupt latency, and is the
Phase 3 starting point; the waker hook replaces it later.

**(d) IOAPIC / ACPI / MP tables.** The VMM writes MP tables / ACPI into guest RAM itself, so the kernel only
needs the **IOAPIC device** (new pure model in `akuma-svm-amd64`: MMIO window 0xFEC00000, 24 redirection
entries, gsi routing default = identity to PIC pin and IOAPIC pin, level/edge, EOI broadcast; ~400 lines +
tests). Neither first guest needs it: Akuma's guest uses LAPIC + PIC (`ExtINT` through LINT0, already the
`Platform::offer` path) and NetBSD MICROVM uses PIC + PIT; Linux is the reason it is on the list.
`SET_GSI_ROUTING` was **not** issued by the census, so v1 uses the default routing table.

**(e) Naming.** Firecracker opens `/dev/kvm`.
* **Option 1 (recommended): a new node `/dev/kvm`, a different protocol, beside `/dev/kvm0`.** kvm0 and herd
  keep working untouched. Cost: one `FileDescriptor` variant carrying a handle into a kernel table (system /
  VM / vCPU), which touches the shared `akuma-exec-core` enum; mitigated by reusing the existing `DevKvm`
  variant plus a side table keyed by (tgid, fd), as the census stub does. v1 rule: **the two nodes are
  mutually exclusive** (the second `CREATE_VM` -> `EBUSY` if a kvm0 VM or another Firecracker VM runs), because
  both own SVM and the reserved cores; a later step can lift that.
* Option 2: make `/dev/kvm0` speak both protocols. Rejected: the write-a-spec protocol and the ioctl protocol
  have different lifetime owners (lease vs fd), and a mistake would break the herd path that works on metal.
* Option 3: an `LD_PRELOAD` shim that translates the ioctls to kvm0's spec. Rejected: kvm0 emulates the devices
  in-kernel while Firecracker emulates them in userspace; a shim cannot supply MMIO/port exits.
* Option 4: put virtio in the kernel and treat Firecracker as a config front-end (`vm_type = firecracker`,
  already reserved in herd). That is the existing kvm0 track, not this task.

**(f) seccomp.** Firecracker installs filters on every thread and panics if `seccomp(2)` fails. Options:
run with `--no-seccomp` (needs nothing from the kernel; recommended for v1, and a plain statement of what is
not enforced), or accept-and-ignore filters (a kernel that claims isolation it does not provide; not
recommended).

### 3. Recommendation and plan

Option 1: new `/dev/kvm` node, new syscall-context run loop, pure ABI crate, kvm0 untouched. Honest size:
**4-6 k lines** over a pure `akuma-kvm-abi` crate (`forbid(unsafe)`, no deps: request decode, struct layouts,
memslot table, 4 KiB NPT encoder, run-page and completion state machine, IOAPIC model, irq routing) and a thin
glue (`amd64/src/kvm/api/`: fd table, VMCB get/set marshalling, the run loop; the only `unsafe` is `VMRUN`
which already exists). Phase 3 slices, each ending in a TCG rehearsal and a doc update:

1. *Plumbing* (replaces the stub's canned answers): system/VM/vCPU fds, memslots + on-demand 4 KiB NPT, vCPU
   state set/get on a real VMCB. Done when Firecracker reaches `KVM_RUN` with the same ioctl sequence as the
   census, with real state.
2. *First exits*: port I/O and MMIO exits and their completion; the run-page layout is pinned by experiment.
   Done when the **Akuma guest's** serial banner appears through Firecracker's emulated 16550.
3. *Interrupts*: PIT/PIC/LAPIC wiring, irqfd/ioeventfd (poll first), virtio-blk driven by Firecracker.
   Done when the Akuma guest reaches its init with a Firecracker-served disk.
4. *Hardening*: `EINTR`/kick, multi-vCPU (one run loop per VMM thread), teardown on any fd closing order,
   unsafe budget and allocation audit, the herd-managed kvm0 VM re-verified after a Firecracker run.
5. *Metal*: ryzen (build on Pop, entry 29/30 recipe), then NetBSD MICROVM.

### 4. Open questions for the owner

1. Approve Option 1 and the v1 exclusivity rule (Firecracker VM xor kvm0 VM)?
2. Accept `--no-seccomp` as the supported mode for v1?
3. ~~Accept "no munmap/mprotect of a live guest-RAM slot"~~ superseded: guest RAM is a borrowed reserve slice (a).

**Owner's answers (2026-10-23): guest RAM is a slice of the `kvmmem=` reserve remapped into the VMM's address space (borrowed frames, kvm0's arena); fork is logged to dmesg; the protocol is learned from the syscall inputs (no Firecracker source read). Phase 3 approved.**

## Phase 3: progress log

Slice 1 (plumbing) started 2026-10-23. Working from this document; entries below record what was built and how it was verified.

### Slice 1a (2026-10-23): handles, one VM, guest RAM on a reserved slice

Built: pure crate `crates/akuma-kvm-abi` (request decode per descriptor kind, `Regs`/`Sregs`/`CpuidEntry`/
`MsrEntry` layouts, the memslot `Table`, `Irqfd`/`Ioeventfd`; 10 host tests whose vectors are the census dumps)
and the glue `amd64/src/kvm/api.rs`: `FileDescriptor::DevKvm(u32)` (0 = `/dev/kvm0`, `n` = handle `n` of the new
node), a static handle table and one static `Vm`; `CREATE_VM` (EBUSY while a kvm0 VM exists, and kvm0's `create`
is EBUSY while an API VM lives; a VM whose owner process is gone is reaped by either), `CREATE_VCPU`,
`GET_SUPPORTED_CPUID` from the host's CPUID through `cpuid::filter`, `CHECK_EXTENSION`, `SET_TSS_ADDR`,
`CREATE_IRQCHIP`/`PIT2` (recorded), `IRQFD`/`IOEVENTFD` (recorded), and **`SET_USER_MEMORY_REGION`**: carve a
2 MiB-aligned slice of the `kvmmem=` reserve and `mm::remap_user_to_phys` moves the VMM's PTEs for the range onto it
(copies present pages, hands the old anonymous frames back through the ledger, maps the rest; the new leaves are
borrowed like `/dev/fb0`'s: not in the ledger, no share count).

Verified (TCG rehearsal, `scripts/kvm/fc_census.sh`, kernel `kvmctl kvmmem=512 kvmapi`, QEMU `-cpu EPYC-Milan,svm=on`):
the unmodified Firecracker walks the same 40 ioctls as the census to `KVM_RUN` with **real answers**, and at
`RUN` the bytes at guest-physical `0x201000` (the PVH entry it loaded the kernel to) read identically through the
slice and through the VMM's own mapping (`SAME`, nonzero); three consecutive runs each carve, run and reap their
slice with no corruption. Not yet: vCPU state (cpuid/msrs/regs/sregs/fpu/lapic are accepted and ignored),
the run loop, and any real exit.

### Slices 1b-3 (2026-10-23): vCPU state, `KVM_RUN`, interrupts — the Akuma kernel boots under Firecracker

**Result (TCG rehearsal, `scripts/kvm/fc_census.sh` with `GUEST_IMG=<akuma_guest_rootfs.sh image>`):** the unmodified
Alpine `firecracker --no-api --no-seccomp --config-file vm.json`, with the Akuma kernel as the guest (PVH ELF, 768 MiB,
one vCPU, a Firecracker-served virtio-blk root), boots to its busybox init. The console shows the guest's banner
through **Firecracker's emulated 16550**, then the guest mounts ext2 on `/dev/vda`, prints `AKUMA-GUEST-UP`, the md5
of `/bin/busybox` read through virtio (equal to the host's `md5` of the same file), writes and re-reads a 256 KiB
file (`/wtest`, md5 printed) and prints `AKUMA-GUEST-DONE`; it ends in `cli; hlt`, which this node reports as
`KVM_EXIT_HLT` and Firecracker logs as "Unexpected exit reason on vcpu run: Hlt" (a real KVM would block).

What was built, in the order it was needed:

* `crates/akuma-kvm-abi` (pure, `forbid(unsafe)`, 12 host tests): request decode per descriptor kind, `Regs`/`Sregs`/
  `CpuidEntry`/`MsrEntry`/FPU layouts (test vectors are the census dumps), the memslot `Table`, `Irqfd`/`Ioeventfd`,
  and `runpage` (below).
* `amd64/src/kvm/api.rs`: `FileDescriptor::DevKvm(id)` handles (system / VM / vCPU), one static `Vm`, reaping by dead
  owner, `GET_SUPPORTED_CPUID` from the host CPUID through `cpuid::filter`, memslots on a reserved slice
  (`mm::remap_user_to_phys`), a 2 MiB-leaf NPT built at the first `RUN`, `mmap` of a vCPU fd as its two run
  frames (`mm::mmap_frames`: borrowed frames, one physical page behind every mapping), IRQFD/IOEVENTFD resolved to the
  kernel's eventfd ids (a reference held until the VM dies).
* `amd64/src/kvm/api/vcpu.rs`: per-vCPU VMCB, host-save frame, GPR file, CPUID table (the VMM's own, 96 entries),
  guest FPU image, and `run`: the world switch **in the VMM thread's syscall** (thread pinned to its core, SVM
  enabled on demand, host FPU saved/restored around `VMRUN`, BKL released only around `VMRUN`, a `yield_now` between
  entries so the VMM's device thread can run, signal check -> `EINTR`). Exits: CPUID from the VMM's table; MSRs;
  PIT/PIC ports in the kernel; every other port, and every MMIO outside the LAPIC page, goes to the VMM through the
  run page and is completed at the next `RUN` (an `IN` or MMIO load finishes when the VMM has filled the data; a
  store finishes at once); registered doorbells signal their eventfd and never leave the kernel; irqfds are polled at
  every entry and every idle wake and raise the PIC pin `gsi` (< 16).
* `amd64/src/kvm/api/platform.rs`: PIT, PIC pair, LAPICs, MSR policy (the `kvm0` arithmetic, minus the 16550 and
  keyboard controller the VMM emulates itself).

**The run-page layout is a hypothesis that Firecracker confirmed by behaviour.** The census cannot show it (only the VMM
reads the page), so it was written from general knowledge of the interface (`exit_reason` at byte 8; the I/O record
`{direction, size, port, count, data_offset}` and the MMIO record `{phys_addr, data[8], len, is_write}` at byte 32),
then tested: a guest `out` to 0x3f8 reaching Firecracker's serial output proves the I/O record, the guest's virtio-blk
driver reading Firecracker's device registers proves the MMIO load/store records and their completion, and a
`doorbell` signalled from the kernel waking Firecracker's device thread proves the eventfd path. Likewise the
`IOEVENTFD` flag bits (datamatch = bit 0, deassign = bit 2) and the `SET_FPU` layout (its 416 bytes add up) are
unconfirmed beyond what this run exercised.

**Bugs found on the way** (each one a silent wedge or wrong answer, none in the pure crate):

1. `destroy_vm` cleared the whole handle table, including the *new* process's system handle (second run: `EBADF`).
   Handles are now reaped by dead owner only.
2. A `/dev/kvm0` create after a Firecracker VM was `EBUSY` until another `CREATE_VM` reaped the dead VM: `vm_live()`
   now reaps a dead owner itself.
3. `KVM_RUN` never yielded: on one core the woken Firecracker device thread never ran (doorbell fired, nothing
   answered, guest waited forever). `yield_now` between entries (with IRQs masked for the switch, as `kvm0` does).
4. The first design held the BKL released for the whole run; the eventfd wake path needs it. The window is now only
   around `VMRUN`.
5. Two overlapping rig runs corrupted a log by editing the script under a running shell (rig lesson, not a kernel bug).

**Not done / open:** the I/O APIC (the guest's ACPI lists one at 0xfec00000; the Akuma guest uses the PIC), `GET/SET_LAPIC`
are accepted and ignored (Firecracker's `SET_LAPIC` LINT setup is therefore not applied: the Akuma guest does not need it),
one vCPU only has been run, no `SET_GSI_ROUTING`, `KVM_RUN` kicks by `tkill` signal are untested, string port I/O is an
`INTERNAL_ERROR`, HLT with `IF=0` is reported as an exit, `--no-seccomp` is required, **and nothing here has run on the
ryzen metal yet**. The guest RAM remap loses a VMM that forks after `CREATE_VM` (logged nowhere yet).

## Metal (ryzen, 2026-10-23)

**Result: the unmodified Alpine `firecracker --no-api --no-seccomp --config-file /root/fc/vm.json` boots the Akuma kernel
as a guest on the ryzen's bare metal**, entry 30 (`kvmctl kvmmem=4096 kvmcores=4-7`), kernel built **on the ryzen** from the
pushed branch, guest kernel `/boot/akuma-guest`, 768 MiB, one vCPU, a 16 MiB ext2 root served by Firecracker's virtio-blk
(`/root/fc/root.img`, a copy of `/boot/akuma-guest-root.img`). The console shows the guest's banner and
`[Block] Registered vda: 16 MB`, `fs: ext2 mounted on /dev/vda`, `AKUMA-GUEST-UP`,
`ebce43017d2cb316ea45e08374de7315  /bin/busybox` (identical to the host's md5 and to the TCG rehearsal's) and
`71277886028e270c53e947eb5989f7f2  /wtest` (the guest-written 256 KiB file, identical to the rehearsal's),
`AKUMA-GUEST-DONE`, then Firecracker's `Unexpected exit reason on vcpu run: Hlt`. First run 43 s (zeroing the 768 MiB
slice and the guest's cold boot); later runs take about a second.

Recipe used (all recorded in [`../runbooks/run-firecracker-on-akuma.md`](../runbooks/run-firecracker-on-akuma.md)):
files staged on p3 while Akuma was up (`/root/fc/{firecracker,vm.json,run.sh}`), `reboot -f` to Pop, fresh clone of the pushed
branch in `~/akuma-metal/fc-kvm` (the existing checkout there is detached with local edits and was left alone; the
`akuma-fbcon` font submodule copied from it), `cargo build -p akuma-amd64 --release --features no-tests`, kernel copied to
p3's `/boot/akuma-amd64.verify` (md5 checked, the old one kept as `.verify.pre-fc-api`), `arm.sh 29`. Herd starts the NetBSD
and `vm-serve-mc` services at that boot and one holds the `/dev/kvm0` slot, so the first `CREATE_VM` was `EBUSY` (the
documented exclusivity); `herd stop` of both freed it.

### Incident: the host died on the fifth Firecracker run (kernel `bb76ea9`)

*Symptom.* Run 1 clean; runs 2 and 3 halted within 0.3 s with no guest output; a later run took the host down:
`[EXCEPTION] #PF page fault err=0 rip=0xffffffff802883c6 cr2=0xfffffffffffffff8 core=1 task_slot=24` at the shell prompt
(photo from the laptop's screen). **Cause (probable, not proven):** `rip` symbolizes, against the very binary that ran
(built on the ryzen), to **`talc::Talc<…PmmOomHandler>::free`**, the kernel heap's free, faulting on a null-minus-8
chunk header: the heap was corrupted. The new code's `RUN` path moved a ~4 KiB `Vcpu` (CPUID table, FPU image) and a
~2 KiB event snapshot by value through several closures; the kernel stack is **32 KiB with no guard page and is itself
heap-allocated**, so an overflow silently overwrites the next heap chunk. That fits a failure that gets worse with each
run, and the immediate halts of runs 2 and 3, but no stack depth was measured. *Fix:* the vCPU is a `Box`, its CPUID
table, FPU image and event snapshot are heap arrays allocated once at `CREATE_VCPU` (fallible), the VM's registrations are
copied in place, and `RUN` moves pointers. *Verified:* TCG, three consecutive runs all pass (before: runs 2 and 3 halted at
once); metal, see the stress result below. **What is still open:** nothing guards a kernel stack (a canary or guard page
would turn the next overflow into a named fault), and the claim "this was the stack" stays a hypothesis until a measurement
says so.

### Metal verification after the fix (kernel `0a27ad8`, same boot)

* **12 consecutive Firecracker runs, 12 passes** (`/root/fc/loop.sh`: each run detached with its output in a file, a run counts
  only with `AKUMA-GUEST-DONE` and both md5s), no `[EXCEPTION]` in `dmesg`, no Firecracker process left behind.
* **The herd-managed `kvm0` path still works afterwards:** `herd start vm-serve-mc`, then `ssh -p 2223 root@<ryzen>` from the Mac
  reaches the guest through the relay (`Akuma akuma-kvm ...`, 2 processors).
* **A rig trap that looked like a kernel bug.** Two Firecracker processes sat idle forever (zero CPU, the VMM's event loop
  alive, the vCPU thread parked) after a test that piped `run.sh` into `grep -q`: grep exits at the first match, the pipe
  closes, and Firecracker's console writes then fail forever. Never judge a run by an early-exiting reader; give the
  output to a file. (The hang was *not* reproduced with output going to a file; the exit and idle tracing added for the hunt
  stays, off the hot path, in `amd64/src/kvm/api/vcpu.rs`.) `kill -9` of such a process freed the VM for the next one.
* The box's own `ssh` client takes no `-o` options (`cannot resolve host '-o'`); use `ssh -p 2223 root@127.0.0.1 'cmd'`, or go
  in from the Mac.

## CPU affinity (design, 2026-10-23)

**Problem.** `KVM_RUN` pins its thread to "whatever core it happens to be on" (`repin_current(me)` in `api/vcpu.rs`), and
the scheduler knows one thing per thread, `Machine::pinned` (a single core or `NO_CPU`). `sched_setaffinity` is a
stub returning 0 in glue; `sched_getaffinity` reports "all online cores" whatever was asked. Firecracker has no pinning
option, so placement has to come from outside the process, the way `taskset` does it.

**Model: one mask per thread, inherited.** `Machine` gains `affinity: AtomicU64` (0 = any core, the default; bit `c` =
core `c`), beside `pinned`, which stays the kernel's *hard* single-core pin (hypervisor run loop). A thread may run on core
`cpu` iff `pinned` allows it **and** `affinity` allows it. The decision is `akuma_vm_spec::affinity::can_run` (pure,
host-tested); `sched::hook_can_run` only loads the three words. A new slot inherits its creator's mask in
`prepare_task_slot` (Linux: fork/clone/exec keep it), so `taskset -c 4-7 cmd` and a herd service need no per-thread work.

**Reserved cores (`kvmcores=`).** Today a reserved core runs only threads *pinned* to it. The rule becomes: a reserved core runs
a thread iff it is pinned there **or its affinity mask names that core**. An unpinned, unmasked thread (sshd, everything
default) never lands on a reserved core, which is the property to keep; a thread that *asks* for core 5 gets it, which is how
a service reaches a reserved core without a kernel special case. Core 0 is never reserved.

**`sched_setaffinity(tid, len, mask)`** (amd64-native arm, not glue's no-op): `tid` 0 = caller, else a Process pid (its
`thread_id` slot). Mask is read as little-endian bytes, bits >= 64 must be zero; the effective mask is the request intersected
with the online cores, empty = `EINVAL` (Linux). A mask covering every online core is stored as 0. Setting the caller's own
mask moves it at once if it stands on a core the mask excludes (park loop as `repin_current`); another thread migrates at its
next switch-out. `sched_getaffinity` returns the stored mask (0 = all online), same byte count rule as before.

**vCPU placement in `/dev/kvm`.** The vCPU thread's own affinity *is* the placement request, so no new ioctl: in `run_pinned`,
`wanted` = the thread's mask (0 = none), `allowed` = `kvmcores=` when set, else all online cores; `cores::place(wanted, allowed,
core_ready, id + 1)[id]` is vCPU `id`'s core (vCPU i -> i-th candidate, wrapping). With no mask and no `kvmcores=` nothing changes:
the thread stays where it is. The thread is moved there with `repin_current` (hard pin for the run, released by `Pinned`),
leaving `affinity` as the VMM set it.

**Herd key `cpus = 4-7`** on any service (including a `command = firecracker ...` one): herd parses it with
`cores::parse`, sets *its own* affinity to the mask around the `spawn` call (the child inherits at slot creation, so no
window in which it runs elsewhere), then restores it. Plain `taskset` works for ad-hoc runs. Mutually exclusive with the
multikernel `core =` key.

**Allocation analysis.** Nothing allocates: a `u64` per slot, byte copies on the stack (<= 128 B mask buffer).

### CPU affinity: built and rehearsed (TCG, 2026-10-23)

*Built as designed above.* Pure: `akuma_vm_spec::affinity` (`decode`, `effective`, `report`, `can_run`; 6 host tests, including
the reserved-core rule and "a hard pin to a core the mask excludes runs nowhere"). Effect: `sched::{affinity_of, set_affinity}`,
`Machine::affinity` inherited in `prepare_task_slot`, `hook_can_run` calling `can_run`; `sched_setaffinity` is a new
`akuma-syscalls-abi` row (203 / 122) with amd64-native arms for both calls in `usermode.rs` (the glue arm stays a no-op for
AArch64); `placed_core` in `api/vcpu.rs`; herd's `cpus` key (`userspace/herd`, `cpus_enter`/`cpus_leave` around the spawn).
`/proc/<pid>/stat` field 39 (`processor`) used to be a constant 0 and now reports the core the thread last ran on
(`akuma-procfs::ProcStat::processor`, from `threading::get_thread_last_core`), which is what makes placement observable.

*Rehearsal* (QEMU TCG `-smp 4`, `smp=4 kvmctl kvmmem=1024 kvmapi kvmcores=2-3`, `scripts/kvm/firecracker_phase0.sh` with `QSMP=4`,
Alpine edge `firecracker` 1.17.0; the rig now links `taskset`/`cut`/`kill` into the guest root):

| run | result |
|---|---|
| `taskset -c 2/3/1-2 sh -c '<spin>; cut -d" " -f39 /proc/self/stat'` | processor `2` / `3` / `2`; `taskset -p` reads back `4`, `8`, `6`; `taskset -c 5 true` fails (`EINVAL`, only 4 cores) |
| `firecracker ...` with no mask, `kvmcores=2-3` | `[KVM] vcpu 0 -> core 2 (thread affinity 0x0, kvmcores 0xc)`; guest banner, both md5s, `AKUMA-GUEST-DONE`, then `Hlt` — about 30 s |
| `taskset -c 3 firecracker ...` | `[KVM] vcpu 0 -> core 3 (thread affinity 0x8, kvmcores 0xc)`; same guest output — about 115 s |

The gap is not a bug in placement: `taskset` on the *whole* VMM puts its API/device threads on the vCPU's core, where they take
turns with the vCPU thread (the `yield_now` between entries), while with no mask they ran on other cores. A first attempt that
sent Firecracker's output to a file inside the guest looked like a hang for the same reason (nothing printed for 25 minutes
before the cap); with the console as stdout it is visibly slow, not stuck. If it matters, the next refinement is to mask only the
vCPU threads (a per-thread `cpus`), which the node can do by itself now that it reads the thread's mask.

*Not yet done:* a thread that is blocked or running elsewhere when its mask changes migrates at its
next switch-out, not at once (only the caller moves immediately); `sched_setaffinity` on a tid that is not a Process thread is `ESRCH`;
herd's `cpus` applies to the spawn only, not to a later `herd` restart of an already-running service's children.

### CPU affinity on the ryzen (metal, 2026-10-23, kernel `0bb4f8c`, built on the ryzen, entry 30, `kvmcores=4-7`)

* `taskset -c 4/5/6/7/4-5 <spin>` ran on core 4 / 5 / 6 / 7 / 5 (processor field of `/proc/self/stat`); `taskset -c 9` is `EINVAL`;
  an unmasked shell stayed on core 3. Reserved cores took only threads that named them.
* **Firecracker as a herd service with `cpus = 6-7`** (`/etc/herd/available/fc-test.conf`, `command = /root/fc/firecracker ...`):
  `[KVM] vcpu 0 -> core 6 (thread affinity 0xc0, kvmcores 0xf0)`; the guest printed its banner, both md5s (`ebce4301...`,
  `71277886...`), `AKUMA-GUEST-DONE`, then Firecracker's `Hlt`. No `[EXCEPTION]` in dmesg.
* After it: `herd start vm-serve-mc` and `ssh -p 2223` into the `kvm0` guest still work.

### Networking under Firecracker: `/dev/net/tun` (2026-10-23, kernel `db4c9e6`, ryzen metal)

`"network-interfaces":[{"iface_id":"eth0","host_dev_name":"tap0",...}]` used to exit before the first `RUN` (`Couldn't open
/dev/net/tun`). Built, in the order Firecracker asked for it:

1. **`/dev/net/tun`** (`amd64/src/kvm/tun.rs`, `FileDescriptor::DevTun`, `FdState::DevTun`, a fifth syscall hook
   `dev_tun_has_frame`): `TUNSETIFF` / `TUNSETVNETHDRSZ` (12) / `TUNSETOFFLOAD` / `TUNGETFEATURES` / `TUNGETIFF`; a frame behind a
   zeroed virtio-net header per `read`; `EPOLLIN` from a relaxed flag the pump and every read/write refresh. The far end is the
   `kvm0` gateway (`akuma_svm_amd64::gateway`: ARP, DHCP 10.0.2.15, ICMP/UDP echo); `tunout` on the kernel command line lets
   guest TCP/DNS out through `kvm/relay.rs`, `tunfwd=<host>:<guest>` forwards one port; a daemon thread polls the relay.
2. **`memfd_create`** (an unlinked `/tmp/.memfd-N`; Firecracker's rx `IovDeque` maps it twice) and **`fcntl(F_ADD_SEALS/F_GET_SEALS)`**
   (accepted, not enforced). New abi row 319 / 279.
3. **`writev`/`readv` on the tap are one frame.** The generic `sys_writev` walked the iovecs with one `write` each, so
   Firecracker's `[12-byte virtio header][frame]` arrived as two writes: a 12-byte runt and a header-less frame the gateway
   ignored (`ignored == frames`). Same family as [`WRITEV_SHORT_WRITE_SPLICE.md`](WRITEV_SHORT_WRITE_SPLICE.md): a vectored call on
   a datagram-like fd is atomic.

*Verified on the ryzen:* the guest finds `eth0` (`AA:FC:00:00:00:01`), DHCP is offered and acknowledged (`dhcp=1/1`,
"DHCP configured", 10.0.2.15/24), ARP is answered, and a UDP datagram to the gateway's port 7 is echoed (`rx_delivered` follows).
Guest DNS to 10.0.2.3 is ignored without `tunout` (expected). `/proc`-style counters: `[tun] writes= reads= eagain= polls= gw: ...`
every 2 s while frames move; `[tun] tx len= ethertype= proto= l4=` for the first 40 guest frames.

*Open — the guest wedges after ~10 s of network activity.* With the NIC attached the guest stops making progress: no more
frames, the guest script never reaches its probe lines, and the host sees `vcpu idle ... due_in_us=none`. The new
`[kvmapi] irq state` line says why it cannot be woken: PIC masks `0xff/0xff` (the Akuma guest polls its virtio devices),
LAPIC nothing pending, LVT timer `0x20` unmasked **one-shot with an initial count that has already expired and is never
re-armed**. So it is not a lost host interrupt: the guest believes it has no timeout and nothing to poll, i.e. it is wedged
inside itself (a lock or a wait on a virtio-net completion). Controls: the same guest with no NIC sleeps 20 s fine on metal;
`kvm0`'s NIC guest (`kvmnet`, a polled model device) works. Next steps: have the guest print from its netpoll/virtio-net path
(`strace_err`, a guest `dmesg`), compare the used-ring/avail-event handling of Firecracker's virtio-net with the model device the
guest driver was written against (`akuma_svm_amd64::virtio_net`: no `VIRTIO_NET_F_CSUM`, 12-byte header, no mergeable buffers;
Firecracker offers more and the guest may mis-negotiate), and run the guest under `kvm0` with Firecracker's device model if
possible. The TCG run of the same scenario is too slow to iterate; the metal cycle (push, Pop build, `arm.sh 29`) is about two
minutes.

### The guest wedge under Firecracker + `/dev/net/tun`: material and theories (2026-10-23, parked)

*Parked on purpose:* Firecracker support is shaky, so the KVM dev cycle ([`AKUMA_KVM_DEV_CYCLE.md`](AKUMA_KVM_DEV_CYCLE.md)) uses `/dev/kvm0`
guests, not Firecracker. Everything needed to resume is here.

**Material.**

* Rig: `scripts/kvm/firecracker_phase0.sh` (`NETDEV=1`, `QSMP=n`, `GUEST_IMG=`), guest image `NET=1 NETPROBE=1 TCPPROBE=1
  scripts/kvm/akuma_guest_rootfs.sh`; on the ryzen `/root/fc/{fcnet.sh,vm-net.json,net-root.img}`; the metal cycle (push, Pop
  build into `~/akuma-metal/fc-target`, p3 `.verify`, `arm.sh 29`) is about two minutes, TCG is far too slow (a guest that boots in
  seconds on metal needs 5+ minutes under TCG).
* Host side counters (`dmesg`): `[tun] writes= reads= eagain= polls= gw: frames= arp= dhcp=1/1 lost= rx_delivered= ignored=`; first 40
  guest frames as `[tun] tx len= ethertype= proto= l4=` (DHCP discover/request, ARP, UDP 7 echo, many UDP 53 = DNS, ignored by design);
  the first 16 ioctls as `[tun] ioctl 0x400454ca / 0x400454d8 / 0x400454d0`.
* Stuck state (`[kvmapi] irq state`, added for this): `pic=0xffff imr=0xff/0xff lapic=0xffff ppr=0 lvt_timer=0x20 count=0xbef6 extint=0`
  and `vcpu idle ... due_in_us=none can_wake=no`; earlier snapshots of the same run showed the timer armed (`due_in_us=875..1747`).
  Last tap traffic: `writes=39 reads=8 eagain=9 polls=23043 arp=5 dhcp=1/1 ignored=31`, then nothing. Exits in the window before the
  wedge: PIT/serial port exits, LAPIC accesses at `0xfee00380` (timer count), `0xfee00320` (LVT timer), `0xfee000b0` (EOI), `irqfd gsi 6`
  eleven times.
* Controls that pass: the same guest with no NIC, `sleep 20` on metal (timers fine); `kvm0` NIC guest (`kvmnet`, the model
  `virtio_net` device, polled); Firecracker + block device only.
* The guest runs with the PIC fully masked: it polls its virtio devices and sleeps on LAPIC one-shots, so a missing wakeup shows as
  "no timer armed", not as an interrupt that never arrived.

**Theories, most likely first** (none confirmed; each has a cheap test).

1. **A guest-side hang in its virtio-net/netpoll path.** The timer fired once and was not re-armed, which means every guest thread
   including the netpoll daemon (which parks with a deadline) is blocked without a deadline: something waits indefinitely, e.g. on a TX
   completion, a lock, or an rx buffer. *Test:* guest `strace_err`/dmesg over the serial console right before the wedge; add a guest
   kernel print in the virtio-net poll path; count used-ring updates Firecracker made vs what the guest consumed.
2. **Feature mismatch.** Firecracker's virtio-net offers more than the model device the guest driver was written against (the model
   offers `VIRTIO_F_VERSION_1` and `VIRTIO_NET_F_MAC` only; Firecracker adds `CSUM`, `GUEST_CSUM`, `GUEST_TSO4/6`, `GUEST_UFO`,
   `HOST_TSO4/6`, `HOST_UFO`, `MRG_RXBUF`, ...). If the guest driver accepts a bit it does not implement (`MRG_RXBUF` changes the header's
   `num_buffers` and lets one frame span buffers), rx can silently misbehave. *Test:* print the offered/accepted feature words in the guest
   driver; mask Firecracker's offer (its net config has no knob, so mask in the guest).
3. **`memfd` double-mapping is not coherent.** Firecracker's rx `IovDeque` maps one memfd twice at adjacent addresses and expects both views
   to alias the same pages. Our `memfd_create` is an unlinked ext2 file mapped `MAP_SHARED` (the `fpcache-rw` path, "first shared writable
   file page published" appears in the log). If the two views are separate frames, the iovec ring Firecracker builds is torn: wrong
   descriptor lengths, a frame written into the wrong buffer, a bogus used length handed to the guest. *Test:* a ten-line static program:
   `memfd_create`, `ftruncate`, `mmap` twice back to back with `MAP_FIXED`, write through one, read through the other, across the
   page boundary.
4. **The 12-byte header on receive.** We hand Firecracker a zeroed header; a real tap fills `num_buffers`. If Firecracker passes our
   header through to a `MRG_RXBUF` guest as is, `num_buffers == 0` is invalid (must be >= 1). *Test:* set byte 10 of the header to 1 in
   `tun::read_frame`.
5. **A lost edge.** Firecracker registers the tap `EPOLLIN` edge-triggered; readiness is a relaxed flag polled by the epoll wait. A frame
   queued while the guest has no free rx buffer is only read when the guest refills the ring and kicks the queue. *Test:* log rx-queue
   kicks vs tap reads; make `HAS_FRAME` re-arm an edge on every `receive`.
6. **Oversize or too-short writes.** `tun::write_frame` drops an oversize frame and returns the full length, `EINVAL`s a runt; a TX
   descriptor chain longer than `BUF` (1526) would be dropped silently (no jumbo frames offered, so unlikely).
7. **Interrupt delivery is irrelevant.** The PIC is masked in the stuck state and the guest polls, so irqfd latency cannot explain a
   permanent stop (it would only slow it down).

*Next experiment when this resumes:* theories 3 and 4 first (cheap, host side), then 2 (guest print), then 1.

*Side finding for the clock/CMOS work:* under Firecracker the guest reads CMOS ports `0x70/0x71` and Firecracker answers
`IO write @ 0x70 failed: bus_error: MissingAddressRange` (no RTC model); the guest logs `clock: CMOS RTC holds no date; waiting
for SNTP`. A Firecracker/`kvm0` guest therefore cannot exercise the real CMOS write path; for the checksum investigation the
suspects stay the host kernel's own CMOS writes (SNTP clock sync, `crumb()` breadcrumbs at byte `0x7F` in `kvm.rs`) and a `reboot -f` mid-write.

## Before the KVM work is finished

In the owner's order of importance (2026-10-23), nothing below is done:

1. **CPU affinity** (built and rehearsed on TCG 2026-10-23, § *CPU affinity: built and rehearsed*; metal result: § *CPU affinity on the ryzen*). (a) Place the vCPU threads of a `/dev/kvm` VM on chosen cores the way `kvm0` places its vCPUs
   (`vm_cores`, `kvmcores=`, `sched::reserve_cores`): a herd key on a `command =` service (a `cpus = 4-7` style list) handed to
   the node, and `repin_current` choosing from it instead of "wherever the thread is". (b) `sched_setaffinity` (today only
   `sched_getaffinity` exists), so `taskset` and any service can be pinned, and a herd key that applies it to the process.
   Firecracker has no pinning option of its own.
2. A **kernel-stack guard** (canary or guard page) and a measurement of the real depth of the `RUN` path.
3. The **I/O APIC** model and `SET_GSI_ROUTING`; `GET/SET_LAPIC` actually applied.
4. **More than one vCPU** (SMP guests: INIT/STARTUP IPIs between vCPU threads), and the `tkill` kick path (`EINTR`) exercised.
5. `HLT` with `IF=0` should block the vCPU like a real KVM instead of exiting; string port I/O; the `IOEVENTFD`
   deassign/port-I/O flags and the run-page extras confirmed or corrected against more of Firecracker's behaviour.
6. An **irqfd waker** instead of polling (interrupt latency is up to a millisecond now).
7. **seccomp**: either `seccomp(2)`/`prctl` accepting filters or `--no-seccomp` stays the documented mode.
8. NetBSD `MICROVM` as the second guest; a Linux guest (needs the I/O APIC).
9. Lift the one-VM exclusivity with `/dev/kvm0` (both own SVM and the reserved cores); a herd service shape for a
   Firecracker VM (`command = firecracker ...`, restart policy) once affinity exists.
10. **Investigate the clock checksum error the ryzen reported when booting Linux (Pop) after Akuma runs** (2026-10-23, reported by the owner, not yet looked at): suspects are what Akuma writes to the CMOS/RTC (the SNTP clock sync, the `crumb()` breadcrumbs in CMOS byte 0x7F that `kvm` uses) and a `reboot -f`/watchdog reset mid-write.
