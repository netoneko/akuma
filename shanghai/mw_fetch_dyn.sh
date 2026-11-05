#!/bin/sh
# Fetch the dynamically linked userland the child probes need, from Alpine's current stable main repository:
# the musl loader, a dynamic busybox (needs only libc), and xz + xz-libs (xz needs liblzma.so.5, a library besides libc).
# (bzip2 is NOT such a binary: Alpine builds libbz2 into it, so it needs only libc. The check at the end verifies the premise.)
#   sh shanghai/mw_fetch_dyn.sh <x86_64|aarch64> <dir>      -> <dir>/root/{lib,usr/lib,usr/bin,bin}, cached
# The package versions float with Alpine's latest-stable; the resolved file names are printed so a result can say which ran.
set -eu
arch=$1; dir=$2
base=https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/$arch
[ -e "$dir/root/lib/ld-musl-$arch.so.1" ] && [ -e "$dir/root/usr/bin/xz" ] && { echo "mw_fetch_dyn: $arch cached in $dir"; exit 0; }
rm -rf "$dir"; mkdir -p "$dir/pkg" "$dir/root"
index=$(curl -sS --max-time 60 "$base/")
for p in musl busybox xz xz-libs; do
  f=$(printf '%s' "$index" | grep -o "href=\"$p-[0-9][^\"]*\\.apk\"" | head -1 | sed 's/href="//; s/"$//')
  [ -n "$f" ] || { echo "mw_fetch_dyn: no $p package in $base" >&2; exit 1; }
  curl -sS --max-time 120 -o "$dir/pkg/$f" "$base/$f"
  tar -xzf "$dir/pkg/$f" -C "$dir/root" 2>/dev/null
  echo "mw_fetch_dyn: $arch $f"
done
[ -e "$dir/root/lib/ld-musl-$arch.so.1" ] && [ -e "$dir/root/usr/bin/xz" ] && [ -e "$dir/root/usr/lib/liblzma.so.5" ] && [ -e "$dir/root/bin/busybox" ]
# The premise of the child probe: xz needs a shared library besides libc, busybox needs only libc.
strings -a "$dir/root/usr/bin/xz" | grep -q '^liblzma\.so' || { echo "mw_fetch_dyn: xz does not name liblzma.so: wrong test subject" >&2; exit 1; }
if strings -a "$dir/root/bin/busybox" | grep -E '^lib[a-z0-9_+.-]+\.so' | grep -v 'libc.musl' | grep -q .; then echo "mw_fetch_dyn: busybox needs more than libc" >&2; exit 1; fi
