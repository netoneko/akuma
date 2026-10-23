# Run the unmodified `firecracker` on Akuma (the `/dev/kvm` node)

**Stability: C.** Akuma's SVM hypervisor also speaks the Linux KVM ioctl protocol on `/dev/kvm`, so Alpine's
`firecracker --no-api --config-file vm.json` boots a guest on it. Verified with the Akuma kernel as the guest, one vCPU,
on QEMU TCG and on the ryzen (2026-10-23). Design, census and findings:
[`../archive/AKUMA_FIRECRACKER_KVM.md`](../archive/AKUMA_FIRECRACKER_KVM.md); invariants:
[`../reference/subsystems/kvm.md`](../reference/subsystems/kvm.md) § *`/dev/kvm`*.

Needs: a kernel booted with `kvmctl kvmmem=<MiB>` (`/dev/kvm` comes with it; the reserve must hold the guest's RAM),
`firecracker` + `libgcc_s` + musl on the host (`apk add firecracker` or copy the binary), a guest kernel (any PVH ELF;
the Akuma kernel works) and a root image (`scripts/kvm/akuma_guest_rootfs.sh`). **`--no-seccomp` is required**
(`seccomp(2)` is `ENOSYS`). **One VM at a time, and not beside a `/dev/kvm0` VM:** stop herd's `vm = true` services first
(`herd stop vm-serve-mc`), or `CREATE_VM` answers `EBUSY` (`Failed to create VM: Resource busy`).

## 1. Rehearse on QEMU TCG (no ryzen)

```sh
cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests
scripts/kvm/akuma_guest_rootfs.sh /tmp/guestroot.img target/x86_64-unknown-none/release/busybox-x86_64
# FCDIR GCCDIR MUSLDIR = extracted apks (firecracker, libgcc, musl); CENSUS_DIR holds fc/ gcc/ musl/
CENSUS_DIR=... GUEST_IMG=/tmp/guestroot.img ROOT_MB=96 \
  BOOT_ARGS="init=/bin/busybox initargs=sh,/etc/guest.sh" REPEATS="1 2 3" SECS=1500 \
  scripts/kvm/fc_census.sh /tmp/fc.log
```

Expect, three times: the guest's banner, `[Block] Registered vda`, `fs: ext2 mounted on /dev/vda`,
`AKUMA-GUEST-UP`, the md5 of `/bin/busybox` (equal to the host's), `71277886028e270c53e947eb5989f7f2  /wtest`,
`AKUMA-GUEST-DONE`, then Firecracker's `Unexpected exit reason on vcpu run: Hlt` (the guest ends in `cli; hlt`).
Do not edit the scripts while a run is alive (the shell reads them as it goes).

## 2. On the ryzen

Build **on the ryzen** (a Mac-built kernel hangs there): from Pop, a fresh clone of the pushed branch (the font
submodule `crates/akuma-fbcon/vendor/spleen` is copied from another checkout), `cargo build -p akuma-amd64 --target
x86_64-unknown-none --release --features no-tests` with `CARGO_TARGET_DIR` set, copy to p3's
`/boot/akuma-amd64.verify`, `sh overlays/ryzen/arm.sh 29` (entry 30, `kvmctl kvmmem=4096`), wait for Akuma
(`ssh -p 2222 root@<wifi address>`). Stage `firecracker`, a `vm.json` and the guest images under `/root/fc/`
(`kernel_image_path = /boot/akuma-guest`, `mem_size_mib = 768`, `vcpu_count = 1`, a read-write drive). Then:

```sh
herd stop vm-netbsd; herd stop vm-serve-mc      # entry 30 starts them; one VM at a time
sh /root/fc/run.sh                              # firecracker --no-api --no-seccomp --config-file /root/fc/vm.json
```

## Verify

The same lines as in § 1, with the host's `dmesg` free of `[EXCEPTION]`; run it ten times in a row. Then
`herd start vm-serve-mc` and `ssh -p 2223 root@<host>` into the herd-managed guest: the `kvm0` path must still work.

## Symptoms

| You see | It is |
|---|---|
| `Error creating KVM object: No such file or directory` | the kernel was not booted with `kvmctl` (no `/dev/kvm`) |
| `Failed to create VM: Resource busy` | a `/dev/kvm0` VM (herd) or another Firecracker VM is live: stop it |
| `Failed to set the requested seccomp filters on vCPU 0` | run with `--no-seccomp` |
| the guest stops at `heap: ... 512 MiB` | the guest needs `mem_size_mib` >= 768 (its fixed 512 MiB heap) |
| `Failed to create VM: Cannot allocate memory` / `guest RAM does not fit the kvmmem= reserve` | no `kvmmem=` (or too small) at boot: guest RAM is a slice of the reserve |
| the host dies with `[EXCEPTION] #PF` in `talc::free` after a few runs | kernel-stack/heap corruption: the 2026-10-23 incident, [`../archive/AKUMA_FIRECRACKER_KVM.md`](../archive/AKUMA_FIRECRACKER_KVM.md) § *Metal* |

## Background

[`../archive/AKUMA_FIRECRACKER_KVM.md`](../archive/AKUMA_FIRECRACKER_KVM.md).
