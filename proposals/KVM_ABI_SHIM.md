# KVM ABI shim: `/dev/kvm` for an unmodified Firecracker (2026-10-23)

Status: **design for approval** (phase 2 of `docs/archive/AKUMA_FIRECRACKER_KVM.md`; nothing here is built
except the logging stub `amd64/src/kvm/api.rs`). Evidence for every statement about what Firecracker
asks for is the phase-1 census in that archive doc. Clean room: nothing below comes from Linux/KVM or
Firecracker source or UAPI headers; names are inferred from request direction/size/behaviour.

## 1. What the census says Akuma must provide

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

## 2. The hard parts, sized

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

## 3. Recommendation and plan

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

## 4. Open questions for the owner

1. Approve Option 1 and the v1 exclusivity rule (Firecracker VM xor kvm0 VM)?
2. Accept `--no-seccomp` as the supported mode for v1?
3. ~~Accept "no munmap/mprotect of a live guest-RAM slot"~~ superseded: guest RAM is a borrowed reserve slice (a).
