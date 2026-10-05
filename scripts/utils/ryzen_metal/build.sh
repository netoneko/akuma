#!/bin/sh
# ryzen bare metal, step 1 of 2 — run as netoneko on ryzen.
#
# Fresh clone of the public repo, kernel built natively twice (a `no-tests`
# kernel for daily use, a plain one that runs the self-test suite — the first
# boots on new hardware want the suite), and the classic `mkdisk.sh` root image.
# Nothing here needs root and nothing outside $W is touched.
#
#   BRANCH=ryzen-wifi sh build.sh        # log: $W/build.log
#
# Step 2 (`install.sh`, root) puts the result on the ESP behind systemd-boot.
# Why this box boots that way: docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md.
export PATH=$HOME/.cargo/bin:$PATH
W=/home/netoneko/akuma-metal
BRANCH=${BRANCH:-ryzen-wifi}
OUT=$W/out
mkdir -p $W $OUT
exec > $W/build.log 2>&1
set -x
date
rm -rf $W/akuma
git clone --depth 1 --branch "$BRANCH" https://github.com/netoneko/akuma.git $W/akuma || exit 1
cd $W/akuma || exit 1
# Only what the kernel and the image need. Not llama.cpp, not the rump tree.
# `--force`: a submodule dir with only `.git` and a matching SHA is otherwise
# left unchecked-out (amd64-bare-metal-loop.md, "Four failures to expect").
git submodule update --init --depth 1 --force \
    crates/akuma-fbcon/vendor/spleen userspace/meow userspace/nca/native-cli-ai
# tinycc: the pinned commit (4597a962) is gone from repo.or.cz — `mob` is a
# force-pushable branch and was rewritten — so no fetch of the remote can
# produce it. It comes from a bundle made on a checkout that still has it:
#   (cd userspace/tcc/tinycc && git bundle create tinycc.bundle mob)
# and copied to $W/tinycc.bundle. Without it the image has no /bin/tcc.
git submodule init userspace/tcc/tinycc
if [ -f $W/tinycc.bundle ]; then
    git clone -q --no-checkout $W/tinycc.bundle userspace/tcc/tinycc
    git -C userspace/tcc/tinycc checkout -q --force "$(git ls-tree HEAD userspace/tcc/tinycc | awk '{print $3}')"
fi
# tcc's build.rs passes clang's `-target`; ryzen's `cc` is gcc.
export CC=clang
git log --oneline -1
rustup target add x86_64-unknown-none

K=target/x86_64-unknown-none/release/akuma-amd64
cargo build -p akuma-amd64 --target x86_64-unknown-none --release || exit 1
cp $K $OUT/akuma-amd64.tests
cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests || exit 1
cp $K $OUT/akuma-amd64
ls -la $OUT

sh amd64/mkdisk.sh $OUT/root.img 512 || exit 1
# The keypair `mkdisk.sh` generated is the only one the image accepts.
cp target/x86_64-unknown-none/release/amd64-ssh-test-key* $OUT/ 2>/dev/null
/sbin/debugfs -R "ls /bin" $OUT/root.img 2>/dev/null | tr -s ' ' '\n' | grep -c .
/sbin/debugfs -R "ls /etc/herd/enabled" $OUT/root.img 2>/dev/null
md5sum $OUT/akuma-amd64 $OUT/akuma-amd64.tests $OUT/root.img
echo "== BUILD DONE $(date)"
