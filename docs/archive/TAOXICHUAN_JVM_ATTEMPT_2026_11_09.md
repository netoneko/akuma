# Can Taoxichuan run a JVM? (aarch64, QEMU HVF, Corretto 21) — 2026-11-09

**Answer:** not the stock kernel, and not yet with the policy lifted. Three blockers in a row; the third is
undiagnosed — ranked suspects from an evening tree walk at the bottom (*Suspects for blocker 3*). Nothing from
this attempt is committed to the kernel.

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

## Suspects for blocker 3 (tree walk, same evening)

Ranked guesses after grepping the tree; nothing here is verified by experiment yet.

1. **mmap error convention** (top suspect). HotSpot reserves the 48M code cache `PROT_NONE`; on failure Linux
   returns `MAP_FAILED` (-1), never `NULL`. If any failure path in `akuma-syscalls-mem/src/mmap.rs` (or the
   syscall arm wrapping it) yields a zero base, musl hands HotSpot a NULL cache base and the first
   stub-generation store goes to `far=0x0` — exactly the observed shape (store to 0 right after the
   `CodeCache:` log line). Grep every mmap return path for a `0` result.
2. **mprotect over `PROT_NONE` reservations, especially to `PROT_EXEC`.** The JIT model is
   reserve-PROT_NONE then commit in place; if committing a sub-range of a lazy/PROT_NONE region errors
   (`akuma-tokio-kernel-core/src/mm.rs`), the cache is unusable and the failure may surface as the NULL store
   rather than a clean VM exit.
3. **Faults never reach the JVM's SIGSEGV handler.** The `[Fault] EL0 ec=0x24 …` line is the kernel's own
   handler; there is no `hs_err` file, so HotSpot's handler — installed precisely to catch probing faults and
   implicit null checks — never ran. If the EL0 data-abort path kills the task without consulting the user
   `rt_sigaction` (SA_SIGINFO ucontext/restorer, `sigaltstack`/SA_ONSTACK), every small HotSpot mismatch becomes
   a raw kill instead of a readable report, and masks the real blocker 3.
4. **Empty `/proc/<pid>/maps`.** `akuma-procfs/src/tree.rs` gates maps/statm rendering on the pid being
   registered with the procfs pseudo-tree, and it rendered empty for the JVM. HotSpot reads `/proc/self/maps`
   during `os::init` (memory info, compressed-oops base heuristics near `libjvm.so`), so this may participate
   in the crash, not just cripple diagnosis.
5. **Page size / auxv mismatch.** This kernel runs 64K pages; Alpine aarch64 musl is built for 4K.
   `akuma-elf-exec/src/stack.rs` does populate `AT_PAGESZ`/`AT_HWCAP`, but verify the value handed to a
   4K-built libc (and that `AT_HWCAP` advertises LSE/CRC) with one `getauxval` probe on the box.
6. **No vDSO.** `akuma-exceptions` mentions it only in a comment. Not a boot blocker (plain syscalls work), but
   HotSpot hammers `clock_gettime(CLOCK_MONOTONIC)`; without a fast path the GC threads crawl. (Aside observed
   on the dev box the same evening: OpenJDK 21 amd64 under a container with vsyscall unavailable segfaults at
   `0xffffffffff600800` in `JNI_CreateJavaVM` — even real kernels bite on this.)

Suggested order of attack: land `getcpu` (correct on its own, see *Policy note*), then a per-process W^X
exemption switch instead of the blunt `None` used here, then route the EL0 fault path through user signal
handlers with VMA-aware fatal reports — the last one likely unmasks blocker 3 for free.

## Policy note

Running any JVM here means giving up W^X (RWX code cache). That is a policy decision, not a bug: the experiment was
built throwaway and not committed. The `getcpu` arm is correct on its own and could land independently.

## Background

`crates/akuma-syscalls-mem/src/wx.rs` (the policy), `crates/akuma-tokio-kernel/README.md` (aarch64 boot recipe),
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`.
