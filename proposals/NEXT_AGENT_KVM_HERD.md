# Next: herd brings up KVM machines (`vm = true`)

> **Status 2026-10-22 (evening):** track 1 is built and TCG-rehearsed (`vm = true`, `/dev/kvm0`,
> `akuma-vm-spec`, `kvmctl`/`kvmmem=`/`kvmcores=`); the metal gate is the open part. See
> [`../docs/reference/subsystems/kvm.md`](../docs/reference/subsystems/kvm.md) § VMs as services. The prompt
> below is kept as written. **Next step (multi-vCPU / multi-VM):** one host task per vCPU, each pinned to
> its own core with its own VMCB and run loop; guest RAM, NPT, device models, gateway and relay behind
> locks or one owner core; a placement policy with a core mask in the spec (VM N gets cores X–Y so host
> services keep some); cross-core delivery of virtual IPIs and interrupts. Today every vCPU time-slices
> inside one task, so more vCPUs add context switches, not compute.

> Original header: written as the next agent's prompt. The idea and its reasoning are in
> [`KVM_UNDER_HERD.md`](KVM_UNDER_HERD.md); this is the task.

## Decisions made after the first draft (2026-10-22, the user)

- **herd is the integration point**, extended in place: it is already what interfaces with boxes.
- **A `/dev/kvm` ioctl API will be needed anyway**, to run Firecracker on Akuma. So do not invent a
  private control file whose shape cannot grow into it. Pick the control surface knowing that. The
  honest difference to design around: the hypervisor today emulates every device **in the kernel**
  (PIC/PIT/LAPIC, virtio-blk, virtio-net behind a built-in gateway, a serial port) and has no
  concept of a **userspace VMM**; Firecracker needs `KVM_CREATE_VM`, `KVM_SET_USER_MEMORY_REGION`,
  `KVM_CREATE_VCPU`, an mmap'd `kvm_run` with `KVM_RUN`, register/sregs/MSR/CPUID get/set, an
  in-kernel irqchip and PIT, `ioeventfd`/`irqfd`, and **userspace exits for MMIO and port I/O**.
  Treat the work as two tracks and say which one you are on: (1) `vm = true` over a kernel-side
  spec, in-kernel devices, one VM, as below — useful on its own; (2) `/dev/kvm` with the KVM ioctl
  ABI and userspace exits, a later and much larger track. Track 1's `Vm` value (RAM, NPT, VMCBs,
  vCPU state separate from the device models) is exactly what track 2 will wrap, so keep the
  device models behind a seam instead of welding them to `run()`.

## The prompt

> TASK: add a **`vm = true` service type to herd** (Akuma's init/supervisor) so a KVM guest is
> brought up, supervised, restarted and logged like any other service — replacing the bootloader
> flag (`kvmserve`/`kvmgentoo`) and the `/boot/kvm-guest` selector file as the way a guest is chosen.
> One VM at a time is enough for this task; do not build multi-VM, `box run --vm`, or a Linux
> guest NIC.
>
> ### Read first (in this order)
>
> 1. `proposals/KVM_UNDER_HERD.md` — the idea, the config sketch, the hypervisor's single-instance
>    statics, the order of work.
> 2. `docs/reference/subsystems/kvm.md` and `docs/archive/AKUMA_KVM_AKUMA_GUEST.md` §§ ssh into the
>    guest, Gentoo as a guest; `docs/archive/KVM_RELAY_SYN_STALL.md` (what the relay and gateway
>    do, what was measured).
> 3. `docs/reference/subsystems/containers.md` § herd and `userspace/herd/src/main.rs`
>    (`ServiceConfig`, the `boxed = true` path, `oneshot`, `restart`, logging to `/var/log/herd`).
> 4. `amd64/src/kvm.rs` (`pick_guest`, `spawn_service`, `serve_task`, the statics) and
>    `amd64/src/kvm/{linux,relay}.rs` (`Guest`, `HostDisk`, `run`, `Relay`).
> 5. `docs/runbooks/kvm-linux-guest.md` — how the guest is staged and verified on the ryzen today.
>
> ### What I want
>
> 1. **A `Vm` control surface in the kernel**, not another boot flag. A small file or syscall
>    (`/proc/kvm/` or `/dev/kvm0` — choose, and say why) that takes a spec: kernel path, cmdline,
>    RAM MiB, vCPUs (1-4), disk path + `ram` (the `HostDisk::Mem` copy) or file, and `forward
>    host:guest` pairs. `create` starts the guest task; `stop` ends it; `status` reports state. The
>    spec parsing is **pure and host-tested** in a crate (`akuma-svm-amd64` is the home, or a new
>    `akuma-kvm-spec`); the control file only carries effects.
> 2. **herd service config keys**: `vm = true`, `vm_kernel`, `vm_disk`, `vm_disk_ram`, `vm_ram_mib`,
>    `vm_vcpus`, `vm_forward` (repeatable), `vm_cmdline`. herd creates the VM, treats the service as
>    running while the guest task is, restarts on exit under the existing `restart`/`max_retries`
>    policy, and writes the guest's console to `/var/log/herd/<name>.log`.
> 3. **Replace the statics** the single-instance code needs for one VM to be *a value*:
>    `SERVICE_GENTOO`, `SERVE_PORT`, `SERVICE_VCPUS`, the `pick_guest(cmdline)` path. The old
>    boot-flag path may stay as a thin shim that builds the same spec, so entry 28 keeps working.
> 4. **Move the ryzen's two guests onto it**: `etc/herd/available/vm-serve.conf` and
>    `vm-gentoo.conf` (`overlays/ryzen/rootfs/`), staged to p3 by hand like `hostname.conf`.
>
> ### Constraints
>
> - AMD SVM only; say so in the config reference. The ryzen is the only metal that runs it.
> - Kernel conventions in `CLAUDE.md`: analyse every new path for allocations (the spec parse and
>   the control file run under memory pressure too), console output through `safe_print!`/`tprint!`,
>   no new `unsafe` (`scripts/check_unsafe_budget.py`).
> - The user drives commits: run clippy and the host tests, leave the tree uncommitted unless asked.
> - Build metal kernels **on the ryzen** (`~/akuma-metal/kvm-build.sh`); a Mac-built kernel has hung
>   on the metal (`docs/archive/AMD64_MAC_BUILT_KERNEL_METAL_HANG.md`). Drive the ryzen with
>   `scripts/utils/hpbox.py rzr`, stage with `overlays/ryzen/kvm-akuma-stage.sh`, arm entry 28.
>
> ### Gates
>
> - Host tests for the spec parser (valid specs, every rejection, limits) and for herd's new config
>   keys.
> - TCG rehearsal: `scripts/kvm/serve_rehearse.py` boots the guest from a herd conf instead of the
>   flag, and passes its stress mode (`--stress 3`).
> - Metal: both ryzen guests come up from herd, `scripts/kvm/relay_stress.py` and
>   `scripts/kvm/relay_bulk.py` pass, `herd status` shows the VM, `kill`-ing the guest task makes
>   herd restart it, and the console log appears in `/var/log/herd`.
> - Document as you go: a reference section in `docs/reference/subsystems/kvm.md` (the control
>   surface and herd keys), a runbook step, an `archive/` doc for anything found on the way, the
>   timeline in `docs/archive/AKUMA_KVM.md`, and a row in `docs/README.md`.
>
> ### Known traps (so you do not rediscover them)
>
> - The hypervisor is one task pinned to **core 0**; vCPUs of one guest take turns on it. The tick
>   accounting shows that core 100% busy while a guest runs; an A/B with no guest uses the same
>   total (28% of 8 cores), so do not chase it as a regression of yours.
> - A guest's NIC has no interrupt; its RX waits for the guest's netpoll. Keep SYN retries generous
>   (4 s): a busy one-vCPU guest answers late, it does not lose SYNs.
> - Guest RAM is one contiguous block (`Ram::alloc`); a second VM will hit fragmentation first —
>   leave that for the multi-VM task.
> - The host's Gentoo box (`conf.d/hostname`) resets the machine name to `localhost` unless its file
>   matches; do not trust a `hostname` oneshot alone.
> - p7 (Gentoo) has ext2 errors and must not be repaired without the owner's say; the guest uses a
>   RAM copy of an image built from it.
