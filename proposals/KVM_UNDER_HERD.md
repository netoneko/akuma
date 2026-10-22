# Idea: KVM machines brought up by herd (and, later, `box`)

> **Status 2026-10-22:** an idea, nothing built. Written down after the Gentoo guest ran on the
> ryzen (`docs/archive/AKUMA_KVM_AKUMA_GUEST.md` § Gentoo as a guest). The decision taken in
> conversation: **herd is the integration point** — it is Akuma's init system, it already
> supervises, restarts and logs services, and it already knows a "boxed" service type. A VM is
> one more service type. `box run --vm` can come after, on top of it.

## 1. Why

Today a KVM guest is a one-off: one per boot, chosen by a bootloader flag (`kvmserve`,
`kvmgentoo`) or by the one word in `/boot/kvm-guest`; its disk is an image staged by hand
(`overlays/ryzen/kvm-gentoo-image.sh`); its only door is one forwarded port (`kvmfwd=`). Firecracker
is the mature alternative and stays the oracle, but it needs a Linux host. The ryzen is an Akuma
host with an SVM hypervisor in its kernel; the missing piece is **lifecycle**, which herd and box
already are.

## 2. Shape

A herd service config, next to `boxed = true`:

    vm = true
    vm_kernel = /boot/akuma-guest        # an Akuma ELF, or a Linux vmlinux
    vm_disk = /boot/gentoo-guest.img     # file; `vm_disk_ram = true` copies it into RAM
    vm_ram_mib = 768
    vm_vcpus = 1
    vm_forward = 2223:4444               # host:guest, repeatable (docker -p)
    vm_cmdline = init=/usr/bin/bash initargs=/akuma-kvm-init.sh
    restart = true

herd asks the kernel to create the VM (a control file or syscall, see §3), supervises it like
any process (the VM is "running" while its task is), restarts it on exit, and keeps its console in
`/var/log/herd/<name>.log` instead of the host's `dmesg`.

## 3. What has to change in the hypervisor

The hypervisor is single-instance by construction:

- statics: `SERVICE_GENTOO`, `SERVE_PORT`, one `Gateway`, `FWD_SLOTS = 2`, the guest picked by
  `pick_guest(cmdline)`; the service task is spawned once, **pinned to core 0**;
- guest RAM is one contiguous 768 MiB block (`Ram::alloc`), which fails by fragmentation for a
  second guest; the NPT maps 2 MiB leaves of that one block;
- the guest's console goes to the host's console ring; the relay's status goes there too.

Needed: a `Vm` value owning RAM, NPT, VMCBs, devices, gateway, relay, and a table of them; scattered
frames behind the NPT; a core-placement policy (not "always 0"); `create / start / stop / list`
control; a console sink per VM; per-VM forwards with more than two slots.

## 4. Order

1. **Make the existing guest quiet and stable first** (the SYN loss, the idle core — see
   `docs/archive/KVM_RELAY_SYN_STALL.md`). N guests would pin N cores.
2. **herd service type** over a `Vm` control file; one VM at a time is enough to prove it.
3. Multi-VM: scattered NPT frames, placement, console per VM.
4. **`box run --vm <image>`**: flatten the image's layers into an ext2 disk (today a hand-run
   `mke2fs -d`), a per-VM disk file for persistence, `box ps` listing VMs next to boxes,
   `box commit` of a VM disk as a single-layer image.
5. A **Linux guest with networking** (the Linux guest boots and mounts a virtio-blk root; it has
   no NIC): that is what makes "any OCI image, unmodified" true and retires the Linux-ABI gaps for
   workloads that need them.

## 4b. Firecracker needs `/dev/kvm`

Added 2026-10-22: running Firecracker (or QEMU) on an Akuma host needs the real KVM ioctl ABI and a
userspace VMM that handles MMIO/port-I/O exits — a different shape from today's all-in-kernel
devices (`NEXT_AGENT_KVM_HERD.md` § Decisions). The herd service type comes first and is useful alone;
design its `Vm` so the device models sit behind a seam a `/dev/kvm` front end can later bypass.

## 5. Open questions

- Persistent disk by default (like a container) or disposable (like today's RAM copy)?
- Guest kernel: Akuma only first, or Linux in scope from step 2?
- Where does the VM table live — herd's, or the kernel's `boxes` table so `box ps` sees it?
- AMD SVM only. Intel VMX is out of scope; say so in the config reference.

## Background

`docs/archive/AKUMA_KVM.md` (timeline), `docs/archive/AKUMA_KVM_AKUMA_GUEST.md`,
`docs/reference/subsystems/kvm.md`, `docs/reference/subsystems/containers.md`.
