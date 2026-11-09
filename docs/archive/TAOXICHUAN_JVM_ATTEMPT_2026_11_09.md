# Can Taoxichuan run a JVM? (aarch64, QEMU HVF, Corretto 21) — 2026-11-09

**Answer:** not the stock kernel, and not yet with the policy lifted. Three blockers in a row; the third is
undiagnosed. Nothing from this attempt is committed to the kernel.

## Rig

`taoxichuan` is x86_64 on bare metal, and QEMU's HVF cannot accelerate an x86_64 guest on an Apple-silicon Mac, so
the **aarch64** build was used (`cargo build -p taoxichuan --target aarch64-unknown-none --release`, the `.bin` is
the Linux Image QEMU `-kernel` takes; recipe in `crates/akuma-tokio-kernel/README.md` § Boot it):

```
qemu-system-aarch64 -machine virt,gic-version=3 -accel hvf -cpu host -smp 4 -m 3072 -kernel taoxichuan.bin \
  -append "init=/bin/sshd initargs=--port,22 smp=4 epoch=$(date +%s)" -nodefaults -no-reboot -display none \
  -serial file:console.log -global virtio-mmio.force-legacy=false \
  -drive file=jvm.img,format=raw,if=none,id=d0 -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.1 \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2245-:22 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.0 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2
```

Boots in seconds, ssh answers, dynamic musl binaries run. The root (900 MB ext2) was built in the Lima `fc` VM:
Alpine 3.21 aarch64 minirootfs + `apk add libstdc++ libgcc zlib` in a chroot, Corretto 21.0.13 Alpine aarch64
(`amazon-corretto-21-aarch64-alpine-jdk.tar.gz`) under `/opt`, Akuma's aarch64 `sshd`, `mke2fs -d`.

## What happened

| # | Kernel | Result |
|---|---|---|
| 1 | stock | `Error occurred during initialization of VM: Failed to mark memory page as executable`. HotSpot probes with an RWX `mprotect`; the kernel answers `EACCES` by design (`akuma-syscalls-mem::wx`: "Programs that need a JIT (V8, a JVM, ...) cannot run under it, and there is no switch or per-program exemption"). Applies to every JVM, Corretto and OpenJ9 alike. |
| 2 | the two W^X checks in `akuma-tokio-kernel-core/src/mm.rs` (`mmap_arm`, `mprotect_arm`) replaced by `None` | `getcpu(2) system call not supported by kernel`. `getcpu` (aarch64 nr 168) is served nowhere in the tree; HotSpot's `os::init_2` exits the VM on `ENOSYS`. |
| 3 | plus a `getcpu` arm in `akuma-tokio-kernel-aarch64/src/syscall.rs` (write `cpu_index()` and node 0 with `copy_to_user`, `EFAULT` on failure) | `-Xlog:all=info` gets through NMT, page-size and metaspace setup, `libjava.so` loads, `CodeCache: min=2496K max=48M ... size=48M`, then `[Fault] EL0 ec=0x24 elr=0x000060000089060c far=0x0 esr=0x92000146` (a store to NULL) and the process is killed. Same with `-Xint -XX:+UseSerialGC -Xss512k -XX:-UsePerfData`. |

## Not found

The crash location. `/proc/<pid>/maps` is empty on this kernel, `trace-err` printed nothing more, and trying
`elr - k*64K` as an offset into `libjvm.so` (text 0..0x12c8d4c) never landed on a store instruction, so it may not be
`libjvm` at all (musl, `libjava`, or the launcher). A NULL store right after the code cache is reserved could be a
failed mapping returning an error that HotSpot then dereferences, or a deliberate `crash_with_segfault`. Next step
would be a fault report that names the module (maps content, or the fault handler printing the VMA), then a Linux
A/B of the same Corretto build with `strace` to compare the syscall sequence.

Eclipse J9 / Semeru was **not** tried: its Alpine/musl builds are x86_64 only as far as checked, and it needs the same
W^X relaxation.

## Policy note

Running any JVM here means giving up W^X (RWX code cache). That is a policy decision, not a bug: the experiment was
built throwaway and not committed. The `getcpu` arm is correct on its own and could land independently.

## Background

`crates/akuma-syscalls-mem/src/wx.rs` (the policy), `crates/akuma-tokio-kernel/README.md` (aarch64 boot recipe),
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`.
