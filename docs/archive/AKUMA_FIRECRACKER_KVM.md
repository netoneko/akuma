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

Written up in [`proposals/KVM_ABI_SHIM.md`](../../proposals/KVM_ABI_SHIM.md): the census mapped onto what
the hypervisor models, the five hard parts sized (guest RAM from the VMM's own memory; `KVM_RUN` in syscall
context; ioeventfd/irqfd; IOAPIC; naming) with options and a recommendation (a new `/dev/kvm` node, a new
run loop reusing the pure models, kvm0 untouched, ~4-6 k lines, five slices). **Waiting for the owner's
approval of the three questions at its end before any Phase 3 code.**
