# denver overlay

Reproduce the experiment's headline result on your own machine: **a Clojure web server running
on Denver**, the platform kernel in [`denver/`](../../denver/README.md), booted under QEMU.
It is the Denver counterpart of [`simple-kernel-devbox`](../simple-kernel-devbox/README.md):
build the kernel from this checkout, build an image, boot, talk to it.

Slides that retell the project: [`bootstrap/public/denver/`](../../bootstrap/public/denver/index.html).

## Quick start

```bash
overlays/denver/build-kernel.sh     # out/denver-amd64      (the gate feature set, ~1 min cold)
overlays/denver/build-image.sh      # out/denver-clojure-amd64.img (JDK 21 + Clojure 1.12 + server.clj)
overlays/denver/run.sh              # boots Denver; Ctrl-A X quits QEMU
curl http://localhost:8080/         # from another terminal
```

Both architectures build and boot (`ARCH=aarch64` or `ARCH=amd64`, default amd64): the kernel, the image and the
runner each take it. On aarch64 the JVM runs with plain 64-bit pointers (`-XX:-UseCompressedClassPointers
-XX:-UseCompressedOops`, set by `run.sh`), because the kernel ignores mmap address hints.

Expected: one line of JSON: `{"server":"clojure 1.12.0","hit":1,"java":"21...","arch":"...","cpus":1,"heap_mib":...}`.

## What you need

| | |
|---|---|
| Rust | the repo's `rust-toolchain.toml` and the `x86_64-unknown-none` target |
| QEMU | `qemu-system-x86_64` (amd64), `qemu-system-aarch64` (aarch64; HVF on Apple Silicon) |
| Docker | only for `build-image.sh`: the image is assembled in an Alpine amd64 container (Alpine's musl JDK, jars from Maven Central, SHA-1 checked) |

## How it works

- **Kernel.** `denver` over `akuma-jvm-kernel` over `akuma-tokio-kernel`, built with the feature set
  the last gate used (`wx-relaxed`, `vsyscall`, `fold-regpreserve`, `sysinfo`, `fault-signal`, `shell`, ...).
  Each one fixes a defect the JVM hit; see [`docs/archive/DENVER_BRANCH_FIXES.md`](../../docs/archive/DENVER_BRANCH_FIXES.md).
- **Guest.** The image holds only the JDK, the musl loader, three jars and `server.clj`. The kernel's
  `init=` is `/opt/jdk/bin/java` itself, so there is no shell and no sshd.
- **Machine.** `-M microvm`, PVH entry, virtio-MMIO, user-mode networking with `8080` forwarded: the same
  local stand-in `amd64/run.sh` uses. There is no `-accel`: on a Mac this is x86 under TCG, correct but slow.
  The experiment itself ran as a KVM guest of Akuma on a Ryzen box, where the JVM starts in seconds.

## Knobs

`ARCH`, `MEMORY` (MiB, default 3072), `SMP` (default 1), `HTTP_PORT` (default 8080), `DENVER_FEATURES` (build-kernel),
`DENVER_DISK`, `DENVER_INIT`, `DENVER_INITARGS` (run).
The disk is attached with `snapshot=on`, so a run never modifies the image.

## Verify

`overlays/denver/check.sh` exits 0 and prints `RESULT PASS` when `curl` got the JSON from the guest.

## Background

- [`denver/README.md`](../../denver/README.md), [`denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md`](../../denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md)
