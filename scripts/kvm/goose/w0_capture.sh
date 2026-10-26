#!/bin/sh
# w0_capture.sh - one wire-struct capture sequence over the guest ssh (stage 8
# trim step 2's W0/W1 baseline). Applies the same two /etc mutations the gate
# battery applies before W1 runs there (G3's apk add, then H2's hostname echo,
# in that order - the ext2 inode allocation must see the same sequence), then
# captures the four probes, hashes them, and cats the hashes back. One exec per
# connection, 15 s apart (RESULT-D finding 3).
#
# Probes (busybox 1.35 in the serve image has NO `stat` applet - two execs
# exited 127 on 2026-10-26; nearest applets per the brief):
#   w1a `ls -lin /etc/hostname` - lstat -> fill_stat64: d_ino, nlink, uid/gid,
#       mode bits, size (the -i/-n flags force the stat fields into the line)
#   w1b `df /`                  - statfs -> fill_statfs block numbers
#   w1c `ls -l /etc`            - getdents64 listing + a stat per entry
#   w1d `uname -a`              - the utsname fields
# Runs the probe block TWICE in this boot and prints both hash sets; identical
# output lines are the within-boot determinism evidence. "CAPTURE LOST" if the
# flaky host->guest client capture never showed a hash (rerun).
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env
OUT=${W0_OUT:-/root/dev-cycle/taoxichuan-s7}
run_ssh() { /bin/ssh -p 2224 root@localhost "$@"; }

run_ssh 'apk add --force-non-repository /pkgs/ca-certificates-bundle-20261020-r0.apk' >"$OUT/w0_m1.client" 2>&1 </dev/null
sleep 15
run_ssh 'sh -c "echo taoxi > /etc/hostname"' >"$OUT/w0_m2.client" 2>&1 </dev/null
sleep 15

hashes() { # $1 = pass suffix; probes into /tmp/w1*, hashes into /tmp/w1h$1
  run_ssh 'ls -lin /etc/hostname > /tmp/w1a' >"$OUT/w0_a$1.client" 2>&1 </dev/null
  sleep 15
  run_ssh 'df / > /tmp/w1b' >"$OUT/w0_b$1.client" 2>&1 </dev/null
  sleep 15
  run_ssh 'ls -l /etc > /tmp/w1c' >"$OUT/w0_c$1.client" 2>&1 </dev/null
  sleep 15
  run_ssh 'uname -a > /tmp/w1d' >"$OUT/w0_d$1.client" 2>&1 </dev/null
  sleep 15
  run_ssh "sha256sum /tmp/w1a /tmp/w1b /tmp/w1c /tmp/w1d > /tmp/w1h$1" >"$OUT/w0_s$1.client" 2>&1 </dev/null
  sleep 15
  n=0
  while [ $n -lt 5 ]; do
    run_ssh "cat /tmp/w1h$1" >"$OUT/w0_h$1.client" 2>&1 </dev/null
    if grep -qE '^[0-9a-f]{64}' "$OUT/w0_h$1.client"; then
      cat "$OUT/w0_h$1.client"
      return 0
    fi
    sleep 15
    n=$((n+1))
  done
  echo "CAPTURE LOST (pass $1)"
  return 1
}

hashes "" || exit 1
hashes 2
