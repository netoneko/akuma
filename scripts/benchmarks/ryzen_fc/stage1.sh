#!/bin/sh
# stage 1 on ryzen, as netoneko: native kernel build + musl-host toolchain
export PATH=$HOME/.cargo/bin:$PATH
W=/home/netoneko/akuma-selfhost
cd $W/akuma || exit 1
{
echo "== $(date) rustup target"; rustup target add x86_64-unknown-none 2>&1 | tail -2
rustc --version
echo "== kernel build"; cargo build -p akuma-amd64 --target x86_64-unknown-none --release 2>&1 | tail -15
ls -la target/x86_64-unknown-none/release/akuma-amd64
echo "== musl toolchain"
rustup toolchain install nightly-x86_64-unknown-linux-musl --profile minimal --force-non-host -c rust-src -t x86_64-unknown-none 2>&1 | tail -5
du -sh ~/.rustup/toolchains/nightly-x86_64-unknown-linux-musl
echo "== vendor"; cargo vendor --versioned-dirs $W/vendor 2>&1 | tail -4
du -sh $W/vendor
echo "== STAGE1 DONE $(date)"
} > $W/stage1.log 2>&1
