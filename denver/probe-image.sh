#!/bin/sh
# denver/probe-image.sh - build the phase-1 rig image: the proven phase-0 root plus the
# workload (the alpine Corretto 21 JRE already on this host) and the jvmprobe binary.
#
#   sh denver/probe-image.sh
#
# The image build trick is the taoxichuan JVM attempt's ("the root ... mke2fs -d"): extract
# the proven 64 MiB image with debugfs, add the JDK + the musl loader + the C++/z libs its
# libjli needs, populate a fresh ext2 from the staging tree. The rig stays outside the tree
# (phase-0 D4): output is /root/denver/w/denver-jdk.img + vm-jdk.spec; the phase-0 rig
# (denver.img, vm.spec) is never touched, so the phase-0 gate stays reproducible.
#
# Not a `#!` script on the box: Akuma's execve cannot run one. Invoke as `sh <file>`.

W=${DENVER_W:-/root/denver/w}
SRC=$W/denver.img
OUT=$W/denver-jdk.img
SPEC=$W/vm-jdk.spec
PROBE=${DENVER_PROBE:-denver/probes/jvmprobe}
BLOCKS=${DENVER_IMG_BLOCKS:-163840}      # 640 MiB at 4 KiB blocks

[ -r "$SRC" ] || { echo "FAIL: no source image at $SRC" >&2; exit 1; }
[ -x "$PROBE" ] || { echo "FAIL: probe not built ($PROBE); run: gcc -static -O2 -o $PROBE denver/probes/jvmprobe.c" >&2; exit 1; }
[ -d /usr/lib/jvm/java-21-openjdk ] || { echo "FAIL: the workload JRE is not on this host" >&2; exit 1; }

STAGE=$(mktemp -d /tmp/denver-img.XXXXXX) || exit 1
trap 'rm -rf "$STAGE"' EXIT

echo "== extract the proven phase-0 image -> $STAGE"
debugfs -R "rdump / $STAGE" "$SRC" 2>/dev/null || { echo "FAIL: debugfs rdump" >&2; exit 1; }

echo "== add /root (sshd's chdir target), /opt, /lib, /usr/lib"
mkdir -p "$STAGE/root" "$STAGE/opt" "$STAGE/lib" "$STAGE/usr/lib" && chmod 700 "$STAGE/root"

echo "== the workload: the host's alpine Corretto 21 JRE -> /opt/jdk (188.8 MiB)"
cp -a /usr/lib/jvm/java-21-openjdk "$STAGE/jdk" && mv "$STAGE/jdk" "$STAGE/opt/jdk" ||
    { echo "FAIL: JDK copy" >&2; exit 1; }

echo "== the musl loader + the libs the JDK's ELF headers name"
cp -a /lib/ld-musl-x86_64.so.1 "$STAGE/lib/" || exit 1
cp -a /usr/lib/libz.so.1* "$STAGE/usr/lib/" || exit 1
cp -a /usr/lib/libstdc++.so.6* "$STAGE/usr/lib/" || exit 1
cp -a /usr/lib/libgcc_s.so.1 "$STAGE/usr/lib/" || exit 1

echo "== the probe suite -> /bin/jvmprobe"
cp "$PROBE" "$STAGE/bin/jvmprobe" && chmod 755 "$STAGE/bin/jvmprobe" || exit 1

echo "== populate ext2 (mke2fs -d, the attempt's trick): $OUT ($((BLOCKS / 256)) MiB)"
rm -f "$OUT"
mke2fs -q -F -t ext2 -b 4096 -d "$STAGE" -E no_copy_xattrs "$OUT" "$BLOCKS" ||
    mke2fs -q -F -t ext2 -b 4096 -d "$STAGE" "$OUT" "$BLOCKS" ||
    { echo "FAIL: mke2fs" >&2; exit 1; }
# (no_copy_xattrs first: this kernel does not implement the xattr syscalls, and mke2fs
# otherwise aborts on its own listxattr — machine fact, Akuma 0.0.9, 2026-11-13)

echo "== fsck it (the phase-0 rig was accepted fsck-clean; same bar)"
e2fsck -f -n "$OUT" || { echo "FAIL: e2fsck" >&2; exit 1; }

echo "== spec: vm-jdk.spec = vm.spec with the new disk"
sed "s|^disk=.*|disk=$OUT|" "$W/vm.spec" > "$SPEC" || exit 1

echo "== sanity: the image sees its own additions"
debugfs -R "ls -l /opt/jdk/bin/java" "$OUT" 2>/dev/null | tail -1
debugfs -R "ls -l /bin/jvmprobe" "$OUT" 2>/dev/null | tail -1
debugfs -R "cat /etc/resolv.conf" "$OUT" 2>/dev/null
echo "IMAGE OK $OUT"
