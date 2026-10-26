#!/bin/sh
# run_gates.sh - the stage-7 gate battery against a booted vm-dev (boot_dev.sh first).
#
# Console-first verdicts: /var/log/herd/vm-dev.log carries the guest's
# "[SSH] Exit status: N" lines, which do not depend on the flaky host->guest
# ssh client capture (RESULT-D finding 3). Where content matters it is turned
# into a guest-side exit code (sha256sum > /tmp/h, then grep -c <hash-prefix>
# /tmp/h - grep exits 0 only on a match), so a lost client capture cannot
# blur a verdict; client capture is kept as a bonus.
#
# Gates: G1 sh -c "echo hi"; G2 apk.static --version; G3 offline
# apk add ca-certificates-bundle + guest-side sha check; G4 ls /etc/ssl1.1/certs;
# H1 busybox wget from the host :8123 listener (started/stopped here, rooted at
# $OUT/www); H2 dd /etc/hostname (created first - not in the image);
# H3a sh -c 'echo x > /tmp/f'; H3b cat /tmp/f; H3c wc -c /tmp/f.
# C1 (stage 8 M1): wall clock - the guest's CLOCK_REALTIME (busybox awk
# systime()) must land in [E, E+900] where E is the epoch boot_dev.sh
# injected into the vm_cmdline (recorded in $OUT/epoch; substituted below).
# D1 (stage 8 M2): the guest's own resolver configuration points at the
# relay's DNS proxy - put there by the image/boot scripts (devroot-ensure
# on the fresh copy; the .orig is never touched).
# D2 (stage 8 M2): ONE `nslookup alpinelinux.org` with NO server argument,
# so the guest's own configuration is used; verdict is the console exit
# status, and the relay's dns_answered counter (vm-dev.log status line)
# rising across the exec is a NOTE line, not the verdict.
# A1/A2/A3 (stage 8 M3): the repositories file on the per-boot COPY
# (devroot-ensure), ONE unflagged `apk update` against the frozen LAN mirror
# (mirror.sh start verifies the manifest first and aborts the run on a
# mismatch; stop at the end), and the downloaded index byte-equal to the
# mirror's APKINDEX.tar.gz (guest-side sha grep, G3's pattern).
# A3 (stage 8 M3) is the mirror cross-check pattern, B1-B4 (stage 8 M4)
# reuse it (below).
# B1-B4 (stage 8 M4): the M4 group runs as a SECOND boot (arg 2 = "b"): the
# main battery is already at the ~30-connection forward ceiling, and the B
# group's 10 execs belong on a fresh guest (apk add curl's 12.6 MB closure
# would also change the disk state W1b's df hash covers). B1 ONE unflagged
# `apk add curl` (the guest's own repositories and keys; the mirror is up
# from the start of the battery) with the mirror request log cross-check;
# B2 `grep -cx curl /etc/apk/world`; B3 the six reference sha256 prefixes as
# guest-side greps (G3's pattern); B4 `apk verify curl` (apk's own integrity
# check of what it unpacked). Nothing unpacked is EXECUTED (curl is a
# dynamic PIE; the loader work is M5).
# Pacing: 15 s between execs, one exec per ssh connection - the host forward
# degrades at ~30 connections per boot. If an exec's console slice shows no
# "[SSHD] Accepted connection" while port 2224 still accepted it, the forward
# is dead: restart the VM ONCE (boot_dev.sh, same kernel, fresh disk) and
# re-run the current gate group from its start, then resume.
#
# One line per gate: "PASS G1" / "FAIL H3b (see h3b.console)" /
# "UNKNOWN G2 (no console exit line)". Evidence: $OUT/<gate>.client +
# <gate>.console; verdict lines also append to $OUT/gates-E.txt.
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env

REPO=/src/github.com/netoneko/akuma
OUT=/root/dev-cycle/taoxichuan-s7
LOG=/var/log/herd/vm-dev.log
KERNEL=${1:-/root/ktarget/x86_64-unknown-none/release/taoxichuan}
GROUP=${2:-main}   # main = G1..W1 battery; b = the M4 apk-add-curl group
SSHPORT=2224
# The host ssh is akuma's own client (no -o flags, pubkey auth only); it must
# be invoked unquoted-word-free, i.e. via this function, never via "$VAR".
run_ssh() { /bin/ssh -p "$SSHPORT" root@localhost "$@"; }
PACE=15
RESTARTED=0
VERD="$OUT/gates-E.txt"
: > "$VERD"

emit() { echo "$*"; echo "$*" >>"$VERD"; }
note() { emit "NOTE $*"; }

fail_exit() { emit "ABORT $*"; exit 1; }
[ -f "$LOG" ] || fail_exit "no console log $LOG - boot_dev.sh first"
[ -f "$KERNEL" ] || fail_exit "no kernel ELF $KERNEL"

# The frozen LAN mirror serves the WHOLE battery: the repositories line is
# live in the image (devroot-ensure, stage-8 M3), so any network-enabled apk
# exec dials :8123. With no listener the relay refuses the dial and NEVER
# tells the guest (gw_refused counts up, resets stays 0) and the guest then
# pins a vCPU at 100% forever - seen 2026-10-26: G3 hung ~13 min on both
# boots, console silent, no Exit status. H1's own listener therefore moved
# to :8124; the mirror owns :8123 for the whole run and is stopped at the
# end. mirror.sh start verifies MANIFEST.sha256 first, aborting on mismatch.
"$REPO/scripts/kvm/goose/mirror.sh" start >>"$OUT/gates-E.err" 2>&1 \
  || fail_exit "mirror.sh start failed (manifest verify?)"

# gate <name> <cmd>: one exec, console-first verdict.
# G_rc: 0 pass (console Exit status: 0), 1 fail (nonzero status), 2 unknown
# (no exit line). G_dead: 1 if the console never showed the accepted conn.
# The console slice must survive a log rotation mid-exec (herd re-parses on
# enable and rotates; seen 2026-10-26 as 0-byte slices and a lost verdict):
# if the active log SHRANK since "before", the rotation happened - take the
# frozen .old's tail plus the fresh active log in full; else the plain delta.
gate() {
  name=$1; cmd=$2
  before_old=$(wc -c < "$LOG.old" 2>/dev/null || echo 0)
  before=$(wc -c < "$LOG")
  run_ssh "$cmd" >"$OUT/$name.client" 2>&1 </dev/null
  sleep "$PACE"   # console drain + connection pacing in one
  if [ "$(wc -c < "$LOG")" -lt "$before" ]; then
    { tail -c +$((before_old+1)) "$LOG.old" 2>/dev/null; cat "$LOG"; } \
      >"$OUT/$name.console" 2>/dev/null
  else
    tail -c +$((before+1)) "$LOG" >"$OUT/$name.console" 2>/dev/null
  fi
  if grep -q 'Exit status: 0' "$OUT/$name.console"; then G_rc=0
  elif grep -q 'Exit status:' "$OUT/$name.console"; then G_rc=1
  else G_rc=2; fi
  if grep -q 'Accepted connection' "$OUT/$name.console"; then G_dead=0; else G_dead=1; fi
}

# runexecs <name=cmd ...>: run the group's execs in order; DEAD=1 if any died.
runexecs() {
  DEAD=0; RCS=""
  for spec in "$@"; do
    name=${spec%%=*}; cmd=${spec#*=}
    gate "$name" "$cmd"
    [ "$G_dead" = 1 ] && DEAD=1
    RCS="$RCS$G_rc"
  done
}

restart_vm() {
  RESTARTED=1
  out=$("$REPO/scripts/kvm/goose/boot_dev.sh" "$KERNEL" 2>>"$OUT/gates-E.err")
  case "$out" in
    "BOOT OK"*) note "VM restarted: $out"; return 0 ;;
    *) note "restart failed: $out"; return 1 ;;
  esac
}

# rgroup <name> <name=cmd ...>: run a group; if the forward died and we have
# not restarted yet, restart once and re-run the group from its start.
rgroup() {
  gname=$1; shift
  runexecs "$@"
  if [ "$DEAD" = 1 ] && [ "$RESTARTED" = 0 ]; then
    note "forward dead during $gname - restarting VM once"
    restart_vm && runexecs "$@"
  fi
}

# agg <rcs>: digit string like "000"; any 1 -> 1, else any 2 -> 2, else 0.
# Split the digits explicitly - "for x in $1" on "000" is ONE word.
agg() {
  r=0
  for x in $(echo "$1" | sed 's/./& /g'); do
    [ "$x" = 1 ] && r=1
  done
  if [ "$r" != 1 ]; then
    for x in $1; do [ "$x" = 2 ] && r=2; done
  fi
  echo $r
}

vline() { # vline <gate> <rc> <evidence>
  case $2 in
    0) emit "PASS $1" ;;
    1) emit "FAIL $1 (see $3)" ;;
    *) emit "UNKNOWN $1 (no console exit line)" ;;
  esac
}

# --- the battery -------------------------------------------------------------
if [ "$GROUP" != main ]; then
  # B group (stage 8 M4), on its own boot: fresh disk, mirror already up
  # (started above). One command per exec - sh cannot fork.
  MLOG=/tmp/alpine-mirror-8123.log
  breq() { grep -c 'GET /main/x86_64/' "$MLOG" 2>/dev/null; }
  B1_BEFORE=$(breq)
  rgroup B1 'b1=apk add curl'
  rcB1=$(agg "$RCS")
  B1_AFTER=$(breq)
  note "mirror apk GETs $B1_BEFORE -> $B1_AFTER"
  [ "${B1_AFTER:-0}" -le "${B1_BEFORE:-0}" ] && rcB1=1
  vline B1 "$rcB1" b1.console

  rgroup B2 'b2=grep -cx curl /etc/apk/world'
  vline B2 "$(agg "$RCS")" b2.console

  # B3: the unpacked bytes equal the Linux reference (guest-side hashes; the
  # prefixes are RESULT-M's reference values). ld-musl: apk may replace the
  # image's own with the same version's bytes - the hash is how we would SEE
  # a replacement; nothing here is executed.
  rgroup B3 \
    'b3a=sha256sum /usr/bin/curl /usr/lib/libcurl.so.4.8.0 /usr/lib/libssl.so.3 /usr/lib/libcrypto.so.3 /usr/lib/libz.so.1.3.2 /lib/ld-musl-x86_64.so.1 > /tmp/h' \
    'b3b=grep -c 9eb8284f /tmp/h' \
    'b3c=grep -c 1ee39403 /tmp/h' \
    'b3d=grep -c ed8d3040 /tmp/h' \
    'b3e=grep -c bb2d86be /tmp/h' \
    'b3f=grep -c ecc8b9df /tmp/h' \
    'b3g=grep -c 095b047c /tmp/h'
  vline B3 "$(agg "$RCS")" "b3a.console b3g.console"

  rgroup B4 'b4=apk verify curl'
  vline B4 "$(agg "$RCS")" b4.console
else
rgroup G1 'g1=sh -c "echo hi"'
vline G1 "$(agg "$RCS")" g1.console

rgroup G2 'g2=apk.static --version'
vline G2 "$(agg "$RCS")" g2.console

# G3: offline apk add, then the hash as guest-side exit codes.
rgroup G3 \
  'g3a=apk add --no-network --force-non-repository /pkgs/ca-certificates-bundle-20261020-r0.apk' \
  'g3b=sha256sum /etc/ssl/certs/ca-certificates.crt > /tmp/h' \
  'g3c=grep -c 5961c0a3 /tmp/h'
vline G3 "$(agg "$RCS")" "g3a.console g3b.console g3c.console"

rgroup G4 'g4=ls /etc/ssl1.1/certs'
vline G4 "$(agg "$RCS")" g4.console

# H1: host listener rooted at $OUT/www, guest fetches through the relay.
# Port 8124: :8123 belongs to the frozen mirror for the whole battery now.
mkdir -p "$OUT/www"
[ -f "$OUT/www/index.html" ] || echo hello-from-host >"$OUT/www/index.html"
(cd "$OUT/www" && exec python3 -m http.server 8124) >/dev/null 2>&1 &
LPID=$!
sleep 1
rgroup H1 \
  'h1a=busybox wget -qO /tmp/h1 http://192.168.1.159:8124/' \
  'h1b=grep -c hello-from-host /tmp/h1'
kill $LPID 2>/dev/null
vline H1 "$(agg "$RCS")" "h1a.console h1b.console"

# H2: /etc/hostname is not in the image - create it first.
rgroup H2 \
  'h2a=sh -c "echo taoxi > /etc/hostname"' \
  'h2b=dd if=/etc/hostname of=/dev/null'
vline H2 "$(agg "$RCS")" "h2a.console h2b.console"

# H3: the owner-approved 3-exec split (no fork arm => no one-line ";" form).
rgroup H3 \
  'h3a=sh -c "echo x > /tmp/f"' \
  'h3b=cat /tmp/f' \
  'h3c=wc -c /tmp/f'
vline H3a "$(echo "$RCS" | cut -c1)" h3a.console
vline H3b "$(echo "$RCS" | cut -c2)" h3b.console
vline H3c "$(echo "$RCS" | cut -c3)" h3c.console

# C1 (stage 8 M1): the wall clock. E is the anchor boot_dev.sh injected at
# THIS boot (re-read here so a mid-battery restart contributes its fresh
# anchor). The brief's form `sh -c 'd=$(date +%s); [ "$d" -ge E ] && ...'`
# cannot run on this kernel: two commands joined by `;`/`&&` and the
# $(date) substitution all need ash to fork, and there is no fork arm
# ("can't fork: Function not implemented"). Single-command equivalent, same
# console-exit-status verdict, same window: busybox awk's systime() reads
# CLOCK_REALTIME, and one simple command tail-call-execs forklessly.
[ -f "$OUT/epoch" ] || fail_exit "no $OUT/epoch - boot_dev.sh did not inject an anchor"
E=$(cat "$OUT/epoch")
rgroup C1 "c1=awk 'BEGIN{exit !(systime()>=$E && systime()<=$E+900)}'"
vline C1 "$(agg "$RCS")" c1.console

# D1 (stage 8 M2): the guest's resolver configuration, as the image/boot
# scripts put it there. One simple command; exit 0 iff the relay's address
# is in /etc/resolv.conf.
rgroup D1 'd1=grep 10.0.2.3 /etc/resolv.conf'
vline D1 "$(agg "$RCS")" d1.console

# D2 (stage 8 M2): one lookup, no server argument - the guest's own
# configuration answers. The verdict stays console-first; the relay's
# dns_answered counter rising across the exec is recorded as a NOTE (the
# counter lives in the log's status line, host side, so a lost guest
# capture cannot blur it).
dnsctr() { grep -o 'dns_answered=[0-9]*' "$LOG" | tail -1 | cut -d= -f2; }
D2_BEFORE=$(dnsctr)
rgroup D2 'd2=nslookup alpinelinux.org'
vline D2 "$(agg "$RCS")" d2.console
D2_AFTER=$(dnsctr)
note "dns_answered $D2_BEFORE -> $D2_AFTER"

# A-group (stage 8 M3): `apk update` in the guest, signed (U1: the three
# official alpine-devel keys ARE in /etc/apk/keys), against the frozen LAN
# mirror - which is ALREADY up (started at the top of the battery; every
# network-enabled apk exec needs it, see the note there). The group sits
# right after D2 per the brief - and before W1's 11 execs, so the new gates
# run at lower connection counts, clear of the ~30-connection forward
# degradation.

# A1: the repositories file is exactly the mirror line - put on the per-boot
# COPY by boot_dev.sh through devroot-ensure; the .orig stays unmodified.
# grep -cx: exit 0 iff one whole line matches (no comment lines left).
rgroup A1 'a1=grep -cx http://192.168.1.159:8123/main /etc/apk/repositories'
vline A1 "$(agg "$RCS")" a1.console

# A2: ONE apk update, no flags - the guest's own repositories and keys.
# Console exit 0 is the verdict; the mirror's request log gaining the index
# GET is cross-checked host-side (a lost client capture cannot fake it).
MLOG=/tmp/alpine-mirror-8123.log
mreq() { grep -c 'GET /main/x86_64/APKINDEX.tar.gz' "$MLOG" 2>/dev/null; }
A2_BEFORE=$(mreq)
rgroup A2 'a2=apk update'
rcA2=$(agg "$RCS")
A2_AFTER=$(mreq)
note "mirror APKINDEX GETs $A2_BEFORE -> $A2_AFTER"
[ "${A2_AFTER:-0}" -le "${A2_BEFORE:-0}" ] && rcA2=1
vline A2 "$rcA2" a2.console

# A3: the downloaded bytes are the mirror's. Judge by content, not name -
# the cache name's 8-hex is a hash of the repository URL, so the guest's
# name differs from the Linux reference's (same sha256 does not).
rgroup A3 \
  'a3a=sha256sum /var/cache/apk/APKINDEX.*.tar.gz > /tmp/h' \
  'a3b=grep -c c169fa06 /tmp/h'
vline A3 "$(agg "$RCS")" "a3a.console a3b.console"

# W1 (stage 8 trim step 2): the guest's own wire-struct bytes must equal the
# pre-refactor W0 baseline. The group is SELF-CONTAINED: it re-applies W0's
# two /etc mutations first (G3's apk add - idempotent, exits 0 re-installed -
# then H2's hostname echo, in that order, so the ext2 inode allocation sees
# W0's sequence even if a mid-battery restart replaced the guest: 2026-10-26,
# a W1 run on a post-restart guest without G3's install failed W1c on the
# missing /etc/ssl). Probes are w0_capture.sh's; the busybox 1.35 in the serve
# image has no `stat` applet (two execs exited 127 that day), so the nearest
# applets stand in: ls -lin /etc/hostname (lstat -> fill_stat64: ino, nlink,
# uid/gid, mode, size), df / (statfs), ls -l /etc (getdents64 + a stat per
# entry), uname -a (utsname). Expected sha256 hashes, one line per probe in
# this order: scripts/kvm/goose/w1_expected_hashes.txt (W0 recorded over three
# fresh boots, identical in-boot second pass). Verdict per probe = guest-side
# exit code (G3's pattern), not client capture.
W1F=$REPO/scripts/kvm/goose/w1_expected_hashes.txt
[ -f "$W1F" ] || fail_exit "no $W1F - W0 baseline missing"
WA=$(sed -n 1p "$W1F"); WB=$(sed -n 2p "$W1F"); WC=$(sed -n 3p "$W1F"); WD=$(sed -n 4p "$W1F")
rgroup W1 \
  'w1m1=apk add --no-network --force-non-repository /pkgs/ca-certificates-bundle-20261020-r0.apk' \
  'w1m2=sh -c "echo taoxi > /etc/hostname"' \
  'w1r1=ls -lin /etc/hostname > /tmp/w1a' \
  'w1r2=df / > /tmp/w1b' \
  'w1r3=ls -l /etc > /tmp/w1c' \
  'w1r4=uname -a > /tmp/w1d' \
  'w1r5=sha256sum /tmp/w1a /tmp/w1b /tmp/w1c /tmp/w1d > /tmp/w1h' \
  "w1a=grep -c $WA /tmp/w1h" \
  "w1b=grep -c $WB /tmp/w1h" \
  "w1c=grep -c $WC /tmp/w1h" \
  "w1d=grep -c $WD /tmp/w1h"
vline W1a "$(echo "$RCS" | cut -c8)" w1a.console
vline W1b "$(echo "$RCS" | cut -c9)" w1b.console
vline W1c "$(echo "$RCS" | cut -c10)" w1c.console
vline W1d "$(echo "$RCS" | cut -c11)" w1d.console

fi  # GROUP=main

"$REPO/scripts/kvm/goose/mirror.sh" stop >>"$OUT/gates-E.err" 2>&1 || true

case "$GROUP" in
  main) emit "GATES DONE restarts=$RESTARTED" ;;
  *)    emit "GATES DONE (group $GROUP) restarts=$RESTARTED" ;;
esac
