#!/bin/bash
# Build the C ext2 probes for both architectures.
#
# These are static musl binaries with no `libakuma`, which is the whole reason
# they are C: the Rust `ext2probe` beside them cannot be built for x86_64, so the
# measurements it carries could not be run on the amd64 kernel at all. The same
# binary also runs on real Linux, which is the reference arm for anything that
# looks like a divergence (`scripts/probes/`, `docs/archive/` on Linux A/B).
#
# Usage:
#   userspace/ext2probe/c/build.sh              # both architectures
#   userspace/ext2probe/c/build.sh x86_64       # just one
set -euo pipefail
cd "$(dirname "$0")"

PROBES="read_syscall_cost pin_reclaim"
ARCHES="${1:-aarch64 x86_64}"

for ARCH in $ARCHES; do
  CC="$ARCH-linux-musl-gcc"
  if ! command -v "$CC" >/dev/null 2>&1; then
    echo "note: $CC not found — skipping $ARCH (brew install FiloSottile/musl-cross/musl-cross)" >&2
    continue
  fi
  # aarch64 binaries land beside the sources (where they always have);
  # x86_64 ones go in `x86_64/`, so the two never overwrite each other.
  OUTDIR="."
  [ "$ARCH" = "x86_64" ] && OUTDIR="x86_64"
  mkdir -p "$OUTDIR"
  for P in $PROBES; do
    [ -f "$P.c" ] || continue
    "$CC" -static -O2 -Wall -Wextra -o "$OUTDIR/$P" "$P.c"
    echo "built $PWD/$OUTDIR/$P ($(wc -c < "$OUTDIR/$P") bytes, $ARCH)"
  done
done
