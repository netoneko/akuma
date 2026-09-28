#!/bin/sh
# stage 2 on ryzen, as root: build the self-host image
W=/home/netoneko/akuma-selfhost; A=$W/akuma; IMG=$W/selfhost.img; M=/mnt/akuma-selfhost
TC=/home/netoneko/.rustup/toolchains/nightly-x86_64-unknown-linux-musl
exec > $W/stage2.log 2>&1
set -x
date
runuser -u netoneko -- sh -c "cd $A && PATH=\$HOME/.cargo/bin:\$PATH sh amd64/mkdisk.sh $IMG 4096" > $W/mkdisk.log 2>&1
echo "mkdisk rc=$?"; tail -30 $W/mkdisk.log
ls -la $IMG
debugfs -R "ls -l /bin" $IMG 2>/dev/null | grep -E "herd|sshd|busybox|paws" 
debugfs -R "ls /lib" $IMG 2>/dev/null
mkdir -p $M; mount -o loop $IMG $M || exit 1
mkdir -p $M/usr/local $M/src/github.com/netoneko
rm -rf $M/usr/local/rust; cp -a $TC $M/usr/local/rust
# musl loader + libgcc_s for the host (proc-macro) linker and librustc_driver
ls $M/lib $M/usr/lib | head -20
cp -a $A $M/src/github.com/netoneko/akuma
rm -rf $M/src/github.com/netoneko/akuma/.git $M/src/github.com/netoneko/akuma/target
cp -a $W/vendor $M/src/github.com/netoneko/akuma/vendor
cat >> $M/src/github.com/netoneko/akuma/.cargo/config.toml <<'CFG'

[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CFG
cp $A/scripts/box/akuma-dev.env $M/etc/akuma-dev.env
cp $A/scripts/box/kbuild $M/bin/kbuild; chmod 755 $M/bin/kbuild
mkdir -p $M/root/.cargo
df -h $M | tail -1
du -sh $M/usr/local/rust
sync; umount $M
echo "== STAGE2 DONE $(date)"
