#!/bin/sh
# Cousin of scripts/benchmarks/ryzen_fc/stage{2,3}.sh, which build the same
# environment into a Firecracker guest's disk *image*; this writes it onto the
# real partition instead.
#
# Stage Akuma's self-hosting development environment onto ryzen's p3, from Pop.
#
#   sh stage-dev.sh              (root, on ryzen, in Pop; p3 not mounted by Akuma)
#
# Does, in order, each step skippable by being already done:
#   1. mount p3 at /mnt/p3 (rw);
#   2. nightly musl toolchain (+ x86_64-unknown-none, rust-src, clippy, rustfmt)
#      from Pop's rustup into /mnt/p3/usr/local/rust, and the two musl `.so`
#      copies lld needs (docs/runbooks/amd64-bare-metal-loop.md, "Four failures");
#   3. Alpine packages onto p3 with apk-tools-static --root: libgcc git make
#      patch less + a monospace font for rio;
#   4. the box rig from the clone at /src/github.com/netoneko/akuma:
#      /etc/akuma-dev.env, /bin/{kbuild,ubuild,mbuild,kinstall}, and the
#      box-local /root/.cargo/config.toml (lld --threads=1, the host linker);
#   5. goose (v1.52.0, /root/stage/goose.tgz) -> /usr/local/bin/goose, its
#      config, and /usr/local/bin/goose-kimi, which reads the Kimi key from
#      /root/.akuma/kimi/token at run time (the key never enters the config);
#   6. rio (/root/stage/rio) -> /bin/rio and its config, if both are staged.
#
# The clone is made by hand beforehand (`git clone --depth=1 --recurse-submodules
# --shallow-submodules https://github.com/netoneko/akuma
# /mnt/p3/src/github.com/netoneko/akuma`); the cargo registry is NOT copied —
# the box fetches crates itself (`kbuild --online`), which is far less data than
# shipping a cache.
set -eu
P=/mnt/p3
SRC=$P/src/github.com/netoneko/akuma
TC=/root/.rustup/toolchains/nightly-x86_64-unknown-linux-musl
STAGE=/root/stage

mountpoint -q $P || { mkdir -p $P; mount /dev/nvme0n1p3 $P; }

echo "== toolchain"
mkdir -p $P/usr/local/bin
# 980 MB; a second run keeps the copy unless FORCE=1.
if [ "${FORCE:-0}" = 1 ] || [ ! -x $P/usr/local/rust/bin/rustc ]; then
    rm -rf $P/usr/local/rust
    cp -a $TC $P/usr/local/rust
fi

echo "== apk"
if [ ! -x $STAGE/apk.static ]; then
    base=https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/x86_64
    f=$(curl -sS $base/ | grep -o 'apk-tools-static-[0-9][^"]*\.apk' | head -1)
    curl -sS -o $STAGE/apk-static.apk $base/$f
    (cd $STAGE && tar xzf apk-static.apk sbin/apk.static 2>/dev/null; mv sbin/apk.static apk.static)
fi
$STAGE/apk.static --root $P --no-scripts --no-cache add libgcc git make patch less font-adobe-source-code-pro
# Same layout as scripts/benchmarks/ryzen_fc/stage3.sh (the Firecracker guest's
# image): the loader doubles as libc.so, and the host linker finds both on
# /usr/lib as well as in lld's own search path.
cp $P/usr/lib/libgcc_s.so.1 $P/usr/lib/libgcc_s.so
cp $P/lib/ld-musl-x86_64.so.1 $P/usr/lib/libc.so
for d in $P/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/lib \
         $P/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained; do
    cp $P/lib/ld-musl-x86_64.so.1 $d/libc.so
    cp $P/usr/lib/libgcc_s.so.1 $d/libgcc_s.so.1
    cp $P/usr/lib/libgcc_s.so.1 $d/libgcc_s.so
done

echo "== rig"
cp $SRC/scripts/box/akuma-dev.env $P/etc/akuma-dev.env
for b in kbuild ubuild mbuild kinstall; do
    cp $SRC/scripts/box/$b $P/bin/$b
    chmod 755 $P/bin/$b
done
mkdir -p $P/root/.cargo
cat > $P/root/.cargo/config.toml <<'CFG'
# Box-local: not in the checkout, so `git status` stays clean.
[target.x86_64-unknown-linux-musl]
linker = "/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin/gcc-ld/ld.lld"
rustflags = ["-C", "link-arg=--threads=1"]
[target.x86_64-unknown-none]
rustflags = ["-C", "link-arg=--threads=1"]
[net]
git-fetch-with-cli = true
CFG

echo "== goose"
if [ -f $STAGE/goose.tgz ]; then
    mkdir -p $STAGE/goose-x && tar xzf $STAGE/goose.tgz -C $STAGE/goose-x
    install -m 755 "$(find $STAGE/goose-x -type f -name goose | head -1)" $P/usr/local/bin/goose
    mkdir -p $P/root/.config/goose $P/root/.akuma/kimi
    chmod 700 $P/root/.akuma $P/root/.akuma/kimi
    cat > $P/root/.config/goose/config.yaml <<'CFG'
GOOSE_TELEMETRY_ENABLED: false
GOOSE_MODE: auto
GOOSE_PROVIDER: openai
GOOSE_MODEL: kimi-for-coding
OPENAI_HOST: https://api.kimi.com
OPENAI_BASE_PATH: coding/v1/chat/completions
CFG
    cat > $P/usr/local/bin/goose-kimi <<'CFG'
#!/bin/sh
# goose against Kimi Code. The key is read here, per run, from a 0600 file;
# it is in no config.
. /etc/akuma-dev.env
OPENAI_API_KEY=$(cat /root/.akuma/kimi/token); export OPENAI_API_KEY
exec /usr/local/bin/goose "$@"
CFG
    chmod 755 $P/usr/local/bin/goose-kimi
fi

echo "== rio"
if [ -f $STAGE/rio ]; then
    install -m 755 $STAGE/rio $P/bin/rio
    mkdir -p $P/root/.config/rio
    [ -f $STAGE/rio-config.toml ] && cp $STAGE/rio-config.toml $P/root/.config/rio/config.toml
fi
sync
echo staged
