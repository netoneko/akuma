#!/bin/sh
# Build the Denver Clojure guest root image without Docker: the same image `build-image.sh` makes
# (JDK 21 musl + Clojure 1.12.0 + denver/samples/clojure/server.clj), with the Alpine pieces taken from an
# Alpine rootfs tarball instead of an `alpine:3.22` container.
#
#   overlays/denver/build-image-nodocker.sh                    -> overlays/denver/out/denver-clojure-amd64.img
#   ALPINE_LAYER=/path/rootfs.tar.gz ALPINE_LIBS_LAYER=/path/other.tar.gz overlays/denver/build-image-nodocker.sh
#
# ALPINE_LAYER defaults to the ecosystem demo's scala box layer (Alpine v3.22 with openjdk21). Needs
# `fakeroot`, `mke2fs`, `e2fsck`, `curl` and `tar` on the host; no root. Jars come from Maven Central and are
# SHA-1 checked against Maven's own .sha1 files. amd64 only (the Clojure server runs on amd64 only).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
OUT=$HERE/out
LAYER=${ALPINE_LAYER:-/var/tmp/akuma-eco/out/layers/scala.tar.gz}
# The scala layer has the JDK and libz; libstdc++/libgcc_s are in any layer with a C++ toolchain (same Alpine release).
LIBS_LAYER=${ALPINE_LIBS_LAYER:-/var/tmp/akuma-eco/out/layers/rust.tar.gz}
[ -r "$LIBS_LAYER" ] || { echo "no libs layer at $LIBS_LAYER (set ALPINE_LIBS_LAYER)" >&2; exit 1; }
[ -r "$LAYER" ] || { echo "no Alpine layer at $LAYER (set ALPINE_LAYER)" >&2; exit 1; }
for t in fakeroot mke2fs e2fsck curl tar; do command -v "$t" >/dev/null || { echo "missing $t" >&2; exit 1; }; done
mkdir -p "$OUT"
cp "$REPO/denver/samples/clojure/server.clj" "$OUT/server.clj"
WORK=$(mktemp -d "${TMPDIR:-/var/tmp}/denver-image.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HERE OUT LAYER LIBS_LAYER WORK

# One fakeroot session so the staged files are root-owned in the image.
fakeroot sh -eu -c '
SRC=$WORK/alpine; S=$WORK/stage
mkdir -p "$SRC" "$S/opt/clojure" "$S/opt/jdk" "$S/lib" "$S/usr/lib" "$S/tmp" "$S/etc" "$S/proc" "$S/dev" "$S/root"
# Only what the image needs out of the layer: the JDK (no jmods/include/sources), musl and its three libraries.
tar xzf "$LAYER" -C "$SRC" \
    --exclude="usr/lib/jvm/java-21-openjdk/jmods" --exclude="usr/lib/jvm/java-21-openjdk/include" \
    --exclude="usr/lib/jvm/java-21-openjdk/lib/src.zip" --exclude="usr/lib/jvm/java-21-openjdk/demo" \
    --exclude="usr/lib/jvm/java-21-openjdk/man" \
    ./usr/lib/jvm/java-21-openjdk ./lib ./usr/lib
tar xzf "$LIBS_LAYER" -C "$SRC" ./usr/lib/libstdc++.so.6 ./usr/lib/libstdc++.so.6.0.33 ./usr/lib/libgcc_s.so.1
cp -a "$SRC/usr/lib/jvm/java-21-openjdk/." "$S/opt/jdk/"
cp -a "$SRC/lib/ld-musl-x86_64.so.1" "$S/lib/"
for l in libz.so.1 libstdc++.so.6 libgcc_s.so.1; do
    f=$(find "$SRC/lib" "$SRC/usr/lib" -maxdepth 1 -name "$l" | head -1)
    [ -n "$f" ] || { echo "missing $l in the layer" >&2; exit 1; }
    cp -aL "$f" "$S/usr/lib/"
done
M=https://repo1.maven.org/maven2/org/clojure
for j in clojure/1.12.0/clojure-1.12.0 spec.alpha/0.5.238/spec.alpha-0.5.238 core.specs.alpha/0.4.74/core.specs.alpha-0.4.74; do
    f=$S/opt/clojure/$(basename $j).jar
    curl -fsSL $M/$j.jar -o "$f"
    want=$(curl -fsSL $M/$j.jar.sha1); have=$(sha1sum "$f" | cut -d" " -f1)
    [ "$want" = "$have" ] || { echo "SHA-1 mismatch: $f" >&2; exit 1; }
done
cp "$OUT/server.clj" "$S/opt/server.clj"
echo "nameserver 10.0.2.3" > "$S/etc/resolv.conf"
rm -f "$OUT/denver-clojure-amd64.img"
mke2fs -q -F -t ext2 -b 4096 -d "$S" -E no_copy_xattrs "$OUT/denver-clojure-amd64.img" 163840
e2fsck -f -n "$OUT/denver-clojure-amd64.img" >/dev/null && echo "image ok"
'
ls -l "$OUT/denver-clojure-amd64.img"
