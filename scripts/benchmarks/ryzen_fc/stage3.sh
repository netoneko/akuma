#!/bin/sh
W=/home/netoneko/akuma-selfhost; IMG=$W/selfhost.img; M=/mnt/akuma-selfhost
exec > $W/stage3.log 2>&1
set -x
B=https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/x86_64
cd $W && mkdir -p apk && cd apk
curl -sSLf -o APKINDEX.tar.gz $B/APKINDEX.tar.gz || exit 1
V=$(tar -xzOf APKINDEX.tar.gz APKINDEX 2>/dev/null | awk '/^P:libgcc$/{f=1} f&&/^V:/{print substr($0,3); exit}')
echo "libgcc version: $V"
curl -sSLf -o libgcc.apk $B/libgcc-$V.apk || exit 1
mkdir -p x && tar -xzf libgcc.apk -C x 2>/dev/null; ls -la x/usr/lib
mount -o loop $IMG $M || exit 1
R=$M/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl
mkdir -p $M/usr/lib
cp x/usr/lib/libgcc_s.so.1 $M/usr/lib/libgcc_s.so.1
cp x/usr/lib/libgcc_s.so.1 $M/usr/lib/libgcc_s.so
cp $M/lib/ld-musl-x86_64.so.1 $M/usr/lib/libc.so
for d in $R/lib $R/lib/self-contained; do
  cp x/usr/lib/libgcc_s.so.1 $d/libgcc_s.so.1; cp x/usr/lib/libgcc_s.so.1 $d/libgcc_s.so
  cp $M/lib/ld-musl-x86_64.so.1 $d/libc.so
done
cat > $M/root/.cargo/config.toml <<'CFG'
[target.x86_64-unknown-linux-musl]
linker = "/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin/gcc-ld/ld.lld"
CFG
ls -la $R/bin/gcc-ld/ $M/usr/lib $R/lib/self-contained | head -30
ls $R/../x86_64-unknown-none/lib | head -5; ls -la $R/../x86_64-unknown-none/lib/libcore*.rmeta
sync; umount $M
echo "== STAGE3 DONE"
