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
