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
