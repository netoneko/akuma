#!/bin/sh
# Boot Shanghai with its sample middleware (and, as a control, without) under QEMU TCG, running
# shanghai/mwprobe/mwprobe.rs as init, and judge each run by the probe's own result line.
#
#   sh shanghai/mw_gate.sh [x86_64|aarch64|all]     (default: all)
#
# Exit 0 only if, on every architecture asked for, the middleware kernel prints "MWPROBE RESULT PASS" and the control
# (the same kernel built without the feature) prints "MWPROBE RESULT CONTROL PASS".
#
#   MW_WORK   work dir (default /tmp/shanghai-mw)
#   SMP       cores for the guest (default 1; on amd64 the rig also puts smp=N on the command line, here too on aarch64)
#   MW_PORT   host port for the amd64 rig's ssh forward, unused with TX_INIT but it must be free (default 2251)
#   E         e2fsprogs sbin dir (default /opt/homebrew/opt/e2fsprogs/sbin)
#
# amd64 boots through scripts/kvm/taoxichuan/tcg_rig.py (stops only the VM it started, by pid file); aarch64 runs
# `qemu-system-aarch64 -machine virt` directly, the line scripts/cargo_runner.sh uses for TCG, and stops it by its pid file.
set -eu
cd "$(dirname "$0")/.."
W=${MW_WORK:-/tmp/shanghai-mw}; E=${E:-/opt/homebrew/opt/e2fsprogs/sbin}; SMP=${SMP:-1}
WHICH=${1:-all}
mkdir -p "$W"

build_probe() { # arch
  rustc --target "$1-unknown-linux-musl" -C linker=rust-lld -C link-self-contained=yes -C target-feature=+crt-static \
    -C relocation-model=static -O shanghai/mwprobe/mwprobe.rs -o "$W/mwprobe-$1" 2>/dev/null
}
image() { # arch
  rm -f "$W/mw-$1.img"; "$E/mke2fs" -q -F -t ext2 -b 4096 "$W/mw-$1.img" 16m
  cat > "$W/cmds-$1" <<EOF
mkdir /bin
mkdir /tmp
write $W/mwprobe-$1 /bin/mwprobe
sif /bin/mwprobe mode 0100755
EOF
  "$E/debugfs" -w -f "$W/cmds-$1" "$W/mw-$1.img" >/dev/null 2>&1
}
wait_result() { # console
  n=0; while [ $n -lt 120 ]; do grep -a -q 'MWPROBE RESULT' "$1" 2>/dev/null && break; n=$((n+1)); sleep 2; done
}
report() { # name arch console want
  echo "== $1 ($2, smp=$SMP)"
  grep -a -E '^(PASS|FAIL|MWPROBE|\[mw\])|PANIC|\[Fault\]' "$3" || echo "(no probe output)"
  grep -a -q "MWPROBE RESULT $4" "$3"
}

# Copy the kernel just built to $W/<name>-<arch>. On aarch64 the flat .bin is made from the ELF here, every time: when cargo
# reuses a cached ELF (switching back to a feature set built earlier) it does not relink, link_kernel.sh does not run, and
# the .bin on disk is the OTHER build's - the control then silently boots the middleware kernel (seen 2026-11-05).
stage() { # name target arch
  if [ "$3" = aarch64 ]; then sh scripts/mkbin.sh "target/$2/release/shanghai" "$W/$1-$3" >/dev/null 2>&1
  else cp "target/$2/release/shanghai" "$W/$1-$3"; fi
}

run_x86_64() { # name kernel want
  export TX_KERNEL=$2 TX_DISK=$W/mw-x86_64.img TX_RUN=$W/run-x86_64-$1 TX_PORT=${MW_PORT:-2251} TX_SMP=$SMP TX_INIT="init=/bin/mwprobe"
  python3 scripts/kvm/taoxichuan/tcg_rig.py boot fresh >/dev/null
  wait_result "$W/run-x86_64-$1/console.log"
  python3 scripts/kvm/taoxichuan/tcg_rig.py stop >/dev/null 2>&1 || true
  report "$1" x86_64 "$W/run-x86_64-$1/console.log" "$3"
}

run_aarch64() { # name kernel.bin want
  d=$W/run-aarch64-$1; rm -rf "$d"; mkdir -p "$d"; cp "$W/mw-aarch64.img" "$d/disk.img"; : > "$d/console.log"
  cl="init=/bin/mwprobe epoch=$(date +%s)"; [ "$SMP" != 1 ] && cl="$cl smp=$SMP"
  qemu-system-aarch64 -machine virt,gic-version=3 -accel tcg -cpu max -smp "$SMP" -m 1024 \
    -kernel "$2" -append "$cl" -nodefaults -no-reboot -display none -serial "file:$d/console.log" \
    -global virtio-mmio.force-legacy=false \
    -drive "file=$d/disk.img,format=raw,if=none,id=d0" -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.1 \
    -netdev user,id=n0 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.0 \
    -pidfile "$d/qemu.pid" -daemonize
  wait_result "$d/console.log"
  [ -f "$d/qemu.pid" ] && kill "$(cat "$d/qemu.pid")" 2>/dev/null || true
  report "$1" aarch64 "$d/console.log" "$3"
}

rc=0
for arch in x86_64 aarch64; do
  [ "$WHICH" = all ] || [ "$WHICH" = "$arch" ] || continue
  build_probe "$arch"; image "$arch"
  tgt=$arch-unknown-none
  cargo build -p shanghai --target "$tgt" --release
  stage plain "$tgt" "$arch"
  cargo build -p shanghai --features middleware --target "$tgt" --release
  stage mw "$tgt" "$arch"
  "run_$arch" control "$W/plain-$arch" 'CONTROL PASS' || rc=1
  "run_$arch" middleware "$W/mw-$arch" 'PASS' || rc=1
done
[ $rc -eq 0 ] && echo "MW GATE PASS ($WHICH, smp=$SMP)" || echo "MW GATE FAIL ($WHICH, smp=$SMP)"
exit $rc
