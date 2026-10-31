export HOME=/root PATH=/usr/local/rust/bin:$PATH
cd /tmp
rm -rf gh tgt
mkdir gh
cd gh
git init -q
git remote add origin https://github.com/netoneko/akuma.git
git fetch -q --depth 1 --filter=blob:none origin 35dbc01adbf8e5162c7d5eb42976788adb5b01d1
git sparse-checkout set --no-cone /Cargo.toml /Cargo.lock /build.rs /linker.ld /.cargo /rust-toolchain.toml /src /amd64 /taoxichuan /crates
git checkout -q FETCH_HEAD
date +%s
cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4 --target-dir /tmp/tgt
date +%s
md5sum /tmp/tgt/x86_64-unknown-none/release/taoxichuan
