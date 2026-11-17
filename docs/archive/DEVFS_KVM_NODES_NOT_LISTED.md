# `/dev/kvm0` and `/dev/kvm` were missing from `ls /dev` (2026-11-17)

## Symptom

On the ryzen, booted with `kvmctl kvmmem=4096 kvmcores=4-7`, "KVM does not work": `ls -al /dev`
showed `fb0` and `wifi0` but no `kvm0` or `kvm`, so the hypervisor looked unwired.

## Investigation

p3 mounted read-only from the Pop host (`udisksctl mount -b /dev/nvme0n1p3 -o ro`; no sudo needed).
The boot log the user had saved (`/no_kvm.log`) and `/var/log/ryzen/klog-297.all` showed the kernel
was fine:

- command line carried `kvmctl kvmmem=4096 kvmcores=4-7` (boot #295 had no `kvmctl`; #296/#297 did);
- `[kvmapi] /dev/kvm armed`, `Fn8000_000A: rev=1 asids=32768` (NP present), `kvmmem: reserved 4096 MiB`,
  `cpu 1..7 svm enabled` (the BSP enables SVM lazily on its first run, so it prints no line).

Nothing was broken in the kernel. The nodes were simply never in the devfs table
(`crates/akuma-vfs/src/dev.rs`): the control nodes are opened by the kernel's own `openat` arm
(`chardev.rs`), which `ls` and `stat` never consult. `kvm.md` recorded this ("`ls /dev` does not
list it") but it misled for the same reason `wifi0` once would have. Separately, no `vm-*` herd
service is enabled on p3, so no guest started at boot.

## Fix

`DevProbe` gained `kvm_ctl` and `kvm_api`; the table gained `kvm0` (`0600`, 10:243) and `kvm`
(`0600`, 10:232). `akuma_vfs_glue::set_kvm_present(ctl, api)` is called from `akuma_kvm_amd64::init`
once the hypervisor is armed. Boxes still see neither (the `in_box` carve-out and the `box_id != 0`
check in `chardev::open` are unchanged). Test: `kvm_nodes_follow_their_probes_and_are_root_only`.

## Verified

Host tests (`akuma-vfs`, 82) pass; amd64 `headless,kvm` checks; the default+`no-tests` metal kernel
built and installed to p3 `/boot/akuma-amd64` (md5 30d8a282144726dd3d8fd0cf660f4891, previous kept as
`.prev`) with `scripts/box/kinstall --build`.

## Still open

Not booted on the ryzen yet: the node listing is unconfirmed on hardware, and no guest has been run
through `/dev/kvm0` since. A reference stays in `docs/reference/subsystems/kvm.md`.
