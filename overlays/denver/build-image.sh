#!/bin/sh
# Build the guest root image: JDK 21 (musl, Alpine) + Clojure 1.12.0 + denver/samples/clojure/server.clj.
#
#   overlays/denver/build-image.sh           -> overlays/denver/out/denver-clojure.img (ext2, 640 MiB)
#
# Needs Docker (the image is assembled in an Alpine amd64 container, so it works from a Mac or
# Linux host alike) and nothing else. Jars come from Maven Central and are SHA-1 checked against
# Maven's own .sha1 files. The kernel has no xattr syscalls, hence `-E no_copy_xattrs`.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
OUT=$HERE/out
mkdir -p "$OUT"
cp "$REPO/denver/samples/clojure/server.clj" "$OUT/server.clj"

docker run --rm --platform linux/amd64 -v "$OUT:/out" alpine:3.22 sh -eu -c '
apk add --no-cache -q openjdk21-jre-headless libstdc++ libgcc zlib e2fsprogs curl >/dev/null
M=https://repo1.maven.org/maven2/org/clojure
S=/stage; mkdir -p $S/opt/clojure $S/opt/jdk $S/lib $S/usr/lib $S/tmp $S/etc $S/proc $S/dev $S/root
for j in clojure/1.12.0/clojure-1.12.0 spec.alpha/0.5.238/spec.alpha-0.5.238 core.specs.alpha/0.4.74/core.specs.alpha-0.4.74; do
    f=$S/opt/clojure/$(basename $j).jar
    curl -fsSL $M/$j.jar -o $f
    want=$(curl -fsSL $M/$j.jar.sha1); have=$(sha1sum $f | cut -d" " -f1)
    [ "$want" = "$have" ] || { echo "SHA-1 mismatch: $f" >&2; exit 1; }
done
cp -a /usr/lib/jvm/java-21-openjdk/. $S/opt/jdk/
cp -a /lib/ld-musl-x86_64.so.1 $S/lib/
for l in libz.so.1 libstdc++.so.6 libgcc_s.so.1; do
    f=$(find /lib /usr/lib -name "$l" | head -1); [ -n "$f" ] || { echo "missing $l" >&2; exit 1; }
    cp -aL "$f" $S/usr/lib/
done
cp /out/server.clj $S/opt/server.clj
echo "nameserver 10.0.2.3" > $S/etc/resolv.conf
rm -f /out/denver-clojure.img
mke2fs -q -F -t ext2 -b 4096 -d $S -E no_copy_xattrs /out/denver-clojure.img 163840
e2fsck -f -n /out/denver-clojure.img >/dev/null && echo "image ok"
'
ls -l "$OUT/denver-clojure.img"
