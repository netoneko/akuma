#!/bin/bash
# Build `mem_op_cost` and optionally push it into a running guest.
#
# The sibling of `userspace/futexprobe/c/build.sh`, and it exists for the same
# two cases a disk-populating build does not cover: pushing a rebuilt probe into
# a VM that is already running (no reboot, no disk edit), and getting the SAME
# binary onto the Linux comparison VM. Building it once here and shipping it to
# both sides is the whole point — see the header of mem_op_cost.c.
#
# Usage:
#   userspace/memprobe/c/build.sh                    # just build
#   userspace/memprobe/c/build.sh --push-akuma 2322  # + push over SSH
#   userspace/memprobe/c/build.sh --push-lima  fc    # + push to a Lima VM
set -euo pipefail
cd "$(dirname "$0")"
# Three probes, and the split is deliberate: `mem_op_cost` arms never fault or
# allocate, `mem_fault_cost` arms do nothing else, and `mmap_scale` measures the
# one thing neither can see — how the cost of a call scales with how many
# mappings the process already holds. Mixing any two would put one's variance on
# top of the other's measurement.
PROBES="mem_op_cost mem_fault_cost mmap_scale"
# Both architectures, on the `userspace/ext2probe/c/build.sh` pattern and for the
# same reason: the amd64 kernel is the one these were written to measure, and a
# probe that only builds for aarch64 cannot be run against it. aarch64 binaries
# land beside the sources (where they always have); x86_64 ones go in `x86_64/`,
# so the two never overwrite each other.
ARCHES="${ARCHES:-aarch64 x86_64}"

for ARCH in $ARCHES; do
  CC="$ARCH-linux-musl-gcc"
  if ! command -v "$CC" >/dev/null 2>&1; then
    echo "note: $CC not found — skipping $ARCH (brew install FiloSottile/musl-cross/musl-cross)" >&2
    continue
  fi
  OUTDIR="."
  [ "$ARCH" = "x86_64" ] && OUTDIR="x86_64"
  mkdir -p "$OUTDIR"
  for OUT in $PROBES; do
    [ -f "$OUT.c" ] || continue
    "$CC" -static -O2 -Wall -Wextra -o "$OUTDIR/$OUT" "$OUT.c"
    echo "built $PWD/$OUTDIR/$OUT ($(wc -c < "$OUTDIR/$OUT") bytes, $ARCH)"
  done
done

case "${1:-}" in
  --push-akuma)
    PORT="${2:-2222}"
    # base64 over SSH: the guest has no scp, and the disk image cannot be
    # written from the host while QEMU holds it open.
    for OUT in $PROBES; do
    base64 < "$OUT" | ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR -p "$PORT" root@localhost \
      "base64 -d > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT"
    done
    ;;
  --push-lima)
    VM="${2:-fc}"
    for OUT in $PROBES; do
    limactl shell "$VM" -- sh -c "cat > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT" < "$OUT"
    done
    ;;
esac
