#!/bin/sh
# Flat-binary conversion for the kernel ELF, shared by the two places that need it:
#
#   scripts/link_kernel.sh  — the `-C linker=` wrapper, so a plain `cargo build`
#                             leaves a .bin next to the ELF (see that script).
#   scripts/cargo_runner.sh — `cargo run`, which regenerates unconditionally
#                             before handing the .bin to QEMU.
#
# Usage: mkbin.sh <src-elf> <dst-bin> [--enforce-size]
#   --enforce-size exits non-zero when the image is over the profile's ceiling;
#   without it an oversize image is reported but tolerated (the linker wrapper
#   uses this: the size policy gate belongs at boot, and a link that "fails"
#   because of a size policy reads as a baffling linker error).
#
# Prints "<bytes> <limit>" on STDOUT (human-readable progress goes to stderr) so
# callers get the ceiling from here rather than hardcoding it a second time —
# cargo_runner.sh needs it for KERNEL_DROPOFF's padding.
#
# POSIX sh, NOT bash, and objcopy is discovered rather than assumed: this runs
# inside the guest during a self-hosted kernel build (acceptance/10), and that
# rootfs has only busybox /bin/sh — no bash — and gets `objcopy` from apk
# binutils rather than rustup's `rust-objcopy`. Same discovery order the
# self-host runbook already documents (docs/runbooks/selfhost-kernel-build.md
# §"objcopy"): rust-objcopy, llvm-objcopy, objcopy.
#
# Why a flat binary at all — QEMU's aarch64 `-kernel` treats a plain ELF as a
# bare-metal image: it jumps to the entry point with x0 = 0 and passes NO device
# tree. The flat image carries an ARM64 Image header (text_offset = 1 MB, see
# linker.ld), which puts QEMU on the Linux boot protocol instead: load at
# RAM_BASE + 1 MB and hand the kernel a DTB pointer in x0. Booting the ELF
# directly was measured 2026-08-26: no FDT, forced single-core, and a kernel OOM
# panic in src/allocator.rs. The .bin is not packaging, it is the boot protocol.
set -e

SRC="${1:?usage: mkbin.sh <src-elf> <dst-bin> [--enforce-size]}"
DST="${2:?usage: mkbin.sh <src-elf> <dst-bin> [--enforce-size]}"

ENFORCE=0
for a in "$@"; do
  [ "$a" = "--enforce-size" ] && ENFORCE=1
done

if [ -n "${OBJCOPY:-}" ]; then
  :
elif command -v rust-objcopy >/dev/null 2>&1; then OBJCOPY=rust-objcopy
elif command -v llvm-objcopy >/dev/null 2>&1; then OBJCOPY=llvm-objcopy
elif command -v objcopy      >/dev/null 2>&1; then OBJCOPY=objcopy
else
  echo "[mkbin] no objcopy found (tried rust-objcopy, llvm-objcopy, objcopy)" >&2
  exit 1
fi

# Temp file + atomic mv so two concurrent runs can never read a half-written
# .bin (the reason the original guard in cargo_runner.sh used this shape).
TMP="${DST}.$$.tmp"
"$OBJCOPY" -O binary "$SRC" "$TMP"
mv -f "$TMP" "$DST"

BYTES=$(wc -c < "$DST" | tr -d ' ')
case "$DST" in
  */extreme-size/*) LIMIT=$((1 * 1024 * 1024)); LABEL="1 MB" ;;
  *)                LIMIT=$((4 * 1024 * 1024)); LABEL="4 MB" ;;
esac

echo "$BYTES $LIMIT"

if [ "$BYTES" -gt "$LIMIT" ]; then
  echo "[mkbin] ERROR: kernel binary is $((BYTES / 1024)) KB, exceeds ${LABEL} limit" >&2
  [ "$ENFORCE" = "1" ] && exit 1
  exit 0
fi
echo "[mkbin] kernel size: $((BYTES / 1024)) KB (limit ${LABEL})" >&2
