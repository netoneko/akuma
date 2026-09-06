#!/bin/sh
# Run the aarch64 kernel inside the Lima VM, under KVM.
#
# Usage, from the macOS host:
#
#     cargo build --release
#     limactl shell fc sh scripts/lima_aarch64_run.sh
#
# Every env var `scripts/cargo_runner.sh` takes works here (`SMP=`, `MEMORY=`,
# `INSTANCE=`, `DISK=`, …) — this is a wrapper around it, deliberately.
#
# # Why the laptop builds and Lima runs
#
# Neither machine can do both:
#
#   - Lima has no Rust toolchain, and the kernel cross-compiles from
#     darwin-aarch64 anyway, which is what this repo does normally.
#   - The laptop's only accelerator is HVF, and HVF **asserts partway through
#     this boot suite** — `Assertion failed: (isv) ... hvf.c`, QEMU exit 134 —
#     on the user-copy EFAULT probe, whose faulting instruction is an LDP and
#     therefore carries no syndrome (`docs/archive/QEMU_HVF_ISV_BUG.md`). TCG is
#     correct and far too slow to sit inside a loop.
#
# Lima's VM has nested virtualisation, so QEMU there gets `/dev/kvm` and
# `cargo_runner.sh` selects it on its own. This script does not pass an
# accelerator, and must not pass anything else either: re-deriving that QEMU
# command line is exactly how the first version of this file booted without
# `-global virtio-mmio.force-legacy=false` and panicked inside the virtio-rng
# driver, on a legacy transport it refuses by design.
#
# # The staging copy
#
# **The Lima mount is read-only**, and the runner regenerates `<elf>.bin` with
# `objcopy` on every launch (unconditionally, and for a good reason — see its
# own comment about a stale `.bin` from a different feature set booting
# silently). So the ELF is copied somewhere writable first. Cheap: ~4 MB, and it
# also pins the run to the binary that existed when it started.
#
# The disk needs no copy. `INSTANCE` defaults to a non-zero value below, which
# makes `cargo_runner.sh` mount the image `snapshot=on` — writes discarded — so
# read-only access is enough, and two runs cannot corrupt each other's mount.
set -e
REPO=$(cd "$(dirname "$0")/.." && pwd)
KERNEL="${KERNEL:-$REPO/target/aarch64-unknown-none/release/akuma}"
STAGE="${STAGE:-/tmp/akuma-lima}"

[ -f "$KERNEL" ] || {
    echo "no kernel at $KERNEL — run 'cargo build --release' on the host first" >&2
    exit 1
}

# Non-zero by default so the disk is snapshotted and the host ports do not
# collide with a devbox on the laptop's own 2222.
INSTANCE="${INSTANCE:-4}"
export INSTANCE

mkdir -p "$STAGE"
cp -f "$KERNEL" "$STAGE/akuma"

# `disk.img` is where `scripts/create_disk.sh` puts it; the runner's own default
# is relative to the working directory, which is not the repo here.
if [ -z "$DISK" ] && [ -f "$REPO/disk.img" ]; then
    DISK="$REPO/disk.img"
    export DISK
fi

exec "$REPO/scripts/cargo_runner.sh" "$STAGE/akuma"
