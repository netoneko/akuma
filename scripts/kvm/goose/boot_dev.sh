#!/bin/sh
# boot_dev.sh [KERNEL_ELF] - one idempotent boot of vm-dev for stage-7 gate work.
#
# Replaces the by-hand dance of RESULT-D ("Ops findings"): stop vm-dev (and
# vm-serve-mc - kvm0 is exclusive), write BOTH herd confs from the tracked
# template (a git-tracked template cannot be clobbered like
# vm-dev.conf.stage7-hostrelay was), disable+enable (herd re-parses a conf
# only then), fresh root image from the .orig, install the kernel ONLY to the
# explicit destination /boot/akuma-guest-dev (never /boot/akuma-amd64*: that
# is the host kernel and it is not rebuilt in this experiment), start, and
# wait for a real ssh round trip (~60 s polls, ~300 s budget; the host ssh is
# akuma's own client - no -o flags, pubkey auth only). Verifies the truth file /var/run/herd/vm-dev.vm (outbound=1,
# disk= the apk image) and the console's "guest disk:" line.
#
# Prints exactly one line:
#   BOOT OK disk=<img> outbound=1 ready_in=<s>
#   BOOT FAIL <reason>
# Detail log: /root/dev-cycle/taoxichuan-s7/boot_dev.log
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env

REPO=/src/github.com/netoneko/akuma
TPL=$REPO/scripts/kvm/goose/vm-dev.stage7.conf
OUT=/root/dev-cycle/taoxichuan-s7
LOG=/var/log/herd/vm-dev.log
RUNF=/var/run/herd/vm-dev.vm
KERNEL=${1:-/root/ktarget/x86_64-unknown-none/release/taoxichuan}
IMG_DST=/boot/akuma-dev-root-ssh.img

mkdir -p "$OUT"
L="$OUT/boot_dev.log"
fail() { echo "BOOT FAIL $1"; exit 1; }

[ -f "$TPL" ]    || fail "no template $TPL"
[ -f "$KERNEL" ] || fail "no kernel ELF $KERNEL"

T0=$(date +%s)
herd stop vm-dev      >>"$L" 2>&1 || true
herd stop vm-serve-mc >>"$L" 2>&1 || true
# The service must be DISABLED for the whole image rewrite below. herd starts
# an enabled-but-stopped service on its next reload tick (~20 s), and the 1 GiB
# copy + devroot-ensure rewrite now take longer than that: the guest then
# opens the image mid-write and reads whatever size the file has reached
# ('guest disk: ... 856 MiB' and '... 10 MiB' for a 1 GiB image,
# docs/archive/KVM_GUEST_DISK_856_MIB_RACE.md). The 16 MiB image copied too
# fast to show it.
herd disable vm-dev >>"$L" 2>&1 || true
cp /boot/akuma-dev-root-apk.img.orig "$IMG_DST" || fail "fresh image copy"
# Stage 8 (M2): the COPY gets the DNS prerequisites from the tree's own
# host-tested ext2 writer (akuma-ext2 via ext2probe's host plumbing): the
# relay resolver in /etc/resolv.conf (the .orig already carries it from the
# rootfs script's SERVE branch; re-asserted) and the /bin/nslookup name (the
# applet is in the image's busybox, but this image build predates the
# symlink line in the SERVE list). The .orig is never touched.
ENSURE=$REPO/userspace/target/x86_64-unknown-linux-musl/release/devroot-ensure
[ -x "$ENSURE" ] || fail "no devroot-ensure binary (cargo build -p ext2probe --bin devroot-ensure --no-default-features --features host-probe --target x86_64-unknown-linux-musl --release, in userspace/)"
"$ENSURE" "$IMG_DST" >>"$L" 2>&1 || fail "devroot-ensure"
"$REPO/scripts/install_kernel_amd64.sh" "$KERNEL" /boot/akuma-guest-dev >>"$L" 2>&1 \
  || fail "kernel install"
# Stage 8 (M1): every boot gets a fresh wall-clock anchor. The tracked
# template stays value-less; the epoch is appended to the vm_cmdline of BOTH
# conf copies and recorded in $OUT/epoch for run_gates.sh's C1 substitution.
EPOCH=$(date +%s)
sed "s/^\(vm_cmdline = .*\)$/\1 epoch=$EPOCH/" "$TPL" > /etc/herd/available/vm-dev.conf \
  || fail "cp available conf"
sed "s/^\(vm_cmdline = .*\)$/\1 epoch=$EPOCH/" "$TPL" > /etc/herd/enabled/vm-dev.conf \
  || fail "cp enabled conf"
grep -q "epoch=$EPOCH" /etc/herd/available/vm-dev.conf || fail "epoch not in available conf"
grep -q "epoch=$EPOCH" /etc/herd/enabled/vm-dev.conf   || fail "epoch not in enabled conf"
echo "$EPOCH" > "$OUT/epoch"
herd enable vm-dev  >>"$L" 2>&1 || fail "herd enable"
herd start vm-dev >>"$L" 2>&1 || fail "herd start"

# Readiness: a real ssh round trip, not console tea-leaves. The host's ssh is
# akuma's own client: no -o flags (it parses them as a hostname), pubkey auth
# only. Poll ~60 s apart, 3 tries (~300 s budget); each try is one sshd
# session, and sessions are the resource the host forward runs out of
# (~30 per boot), so keep the count minimal.
ready=""
i=0
while [ $i -lt 3 ]; do
  [ $i -gt 0 ] && sleep 60
  rm -f "$OUT/boot_probe.out"
  /bin/ssh -p 2224 root@localhost 'echo __BOOT_OK__' >"$OUT/boot_probe.out" 2>>"$L"
  if grep -q '__BOOT_OK__' "$OUT/boot_probe.out"; then
    # herd re-parses on enable and QUEUES a start on the next config reload
    # (within 20 s): a second guest can replace this one right after the first
    # answer. Let that window pass, then confirm the survivor still answers.
    sleep 25
    rm -f "$OUT/boot_probe.out"
    /bin/ssh -p 2224 root@localhost 'echo __BOOT_OK__' >"$OUT/boot_probe.out" 2>>"$L"
    grep -q '__BOOT_OK__' "$OUT/boot_probe.out" && ready=1
  fi
  [ "$ready" = 1 ] && break
  i=$((i+1))
done
[ "$ready" = 1 ] || fail "no ssh round trip (see boot_dev.log)"

# Stage 8 (M1): $OUT/epoch must describe the guest that ANSWERED, not the conf
# we wrote: herd's reload tick can start the survivor from a spec parsed a
# moment before our conf landed (seen live 2026-10-26: conf/boot_dev said
# ...9265, the booted guest carried ...8780). The survivor's spec file
# (/var/run/herd/vm-dev.vm) is the ground truth - sync the anchor from it.
LIVE=$(sed -n 's/.* epoch=\([0-9]*\).*/\1/p' "$RUNF" 2>/dev/null | tail -1)
case "$LIVE" in
  ''|*[!0-9]*) fail "no epoch= in $RUNF (guest predates the anchor)" ;;
  *) echo "$LIVE" > "$OUT/epoch" ;;
esac

grep -q '^outbound=1' "$RUNF" 2>/dev/null || fail "runfile missing outbound=1"
grep -q "^disk=$IMG_DST" "$RUNF" 2>/dev/null || fail "runfile disk is not the apk image"
# The console log rotates when herd re-parses mid-boot (seen 2026-10-26: the
# boot's own 'guest disk:' line landed in vm-dev.log.old while the check read
# the fresh file). Search both sides of the last rotation for the line.
grep -h 'guest disk:' "$LOG" "$LOG.old" 2>/dev/null | tail -1 | grep -q "$IMG_DST" \
  || fail "console guest disk line wrong"

READY=$(( $(date +%s) - T0 ))
echo "BOOT OK disk=$IMG_DST outbound=1 ready_in=${READY}s"
