#!/bin/sh
# Boot older kernels against today's root.img in the QEMU rehearsal — run as root
# on ryzen, after build.sh. Kernel only: userspace stays fixed, so a verdict
# change between two commits is a kernel change.
#
#   sh bisect.sh [entry] <commit>...     # e.g. sh bisect.sh 1 87f1a7c5 5ceb3bb0 9fd74c05
#
# Each commit is built `--features no-tests` in a worktree of $W/akuma, sharing
# one target dir, and gets one line: how many herd services started, whether an
# exception was printed, the serial log's last line, and qemu's exit.
W=/home/netoneko/akuma-metal
A=$W/akuma
ENTRY=$1; shift
B=$W/bisect
mkdir -p $B
chown netoneko: $B
runuser -u netoneko -- git -C $A fetch -q --deepen=60 origin 2>/dev/null
for c in "$@"; do
    K=$B/akuma-amd64.$c
    if [ ! -s $K ]; then
        runuser -u netoneko -- sh -c "
            export PATH=\$HOME/.cargo/bin:\$PATH
            rm -rf $B/wt; git -C $A worktree prune
            git -C $A worktree add -q --detach $B/wt $c || exit 1
            rmdir $B/wt/crates/akuma-fbcon/vendor/spleen 2>/dev/null
            ln -sfn $A/crates/akuma-fbcon/vendor/spleen $B/wt/crates/akuma-fbcon/vendor/spleen
            cd $B/wt && CARGO_TARGET_DIR=$B/target cargo build -q -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests 2>&1 | grep -E '^error' | head -3
            cp $B/target/x86_64-unknown-none/release/akuma-amd64 $K"
    fi
    [ -s $K ] || { echo "$c: BUILD FAILED"; continue; }
    S=$(date +%s)
    RC=$(KERNEL=$K sh $A/overlays/ryzen/qemu.sh $ENTRY 60 std 2>&1 | sed -n 's/^qemu rc=\([0-9]*\).*/\1/p')
    L=$W/qemu/serial-$ENTRY.log
    cp $L $B/serial-$c.log
    echo "$c: starts=$(grep -a -c 'Starting service' $L) exc=$(grep -a -c EXCEPTION $L) rc=$RC $(( $(date +%s) - S ))s last=[$(tail -1 $L | tr -d '\r' | cut -c1-70)]"
done
