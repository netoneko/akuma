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
#
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

# gate <name> <cmd>: one exec, console-first verdict.
# G_rc: 0 pass (console Exit status: 0), 1 fail (nonzero status), 2 unknown
# (no exit line). G_dead: 1 if the console never showed the accepted conn.
gate() {
  name=$1; cmd=$2
  before=$(wc -c < "$LOG")
  run_ssh "$cmd" >"$OUT/$name.client" 2>&1 </dev/null
  sleep "$PACE"   # console drain + connection pacing in one
  tail -c +$((before+1)) "$LOG" >"$OUT/$name.console" 2>/dev/null
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
rgroup G1 'g1=sh -c "echo hi"'
vline G1 "$(agg "$RCS")" g1.console

rgroup G2 'g2=apk.static --version'
vline G2 "$(agg "$RCS")" g2.console

# G3: offline apk add, then the hash as guest-side exit codes.
rgroup G3 \
  'g3a=apk add --force-non-repository /pkgs/ca-certificates-bundle-20261020-r0.apk' \
  'g3b=sha256sum /etc/ssl/certs/ca-certificates.crt > /tmp/h' \
  'g3c=grep -c 5961c0a3 /tmp/h'
vline G3 "$(agg "$RCS")" "g3a.console g3b.console g3c.console"

rgroup G4 'g4=ls /etc/ssl1.1/certs'
vline G4 "$(agg "$RCS")" g4.console

# H1: host listener rooted at $OUT/www, guest fetches through the relay.
mkdir -p "$OUT/www"
[ -f "$OUT/www/index.html" ] || echo hello-from-host >"$OUT/www/index.html"
(cd "$OUT/www" && exec python3 -m http.server 8123) >/dev/null 2>&1 &
LPID=$!
sleep 1
rgroup H1 \
  'h1a=busybox wget -qO /tmp/h1 http://192.168.1.159:8123/' \
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

emit "GATES DONE restarts=$RESTARTED"
