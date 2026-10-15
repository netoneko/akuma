#!/bin/sh
# Gentoo as box 0 on Akuma: the kernel runs this as init (init=/usr/bin/bash initargs=/akuma-init.sh).
#
# The kernel hands init the framebuffer console (stdout/stderr) and the keyboard
# (stdin). Keep the screen on fds 3/4 before the logs take 1/2: the console shell
# at the bottom runs on them, which is what makes the cursor on the TV useful.
exec 3>&1 4>&2
# Logs APPEND (O_APPEND): spawned children do not share an inherited fd's file
# offset on Akuma (docs/archive/AKUMA_GENTOO_PORTAGE_CRASH.md), so a plain `>`
# log is torn by the background jobs.
: > /akuma-init.log
exec >> /akuma-init.log 2>&1
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root TERM=xterm
mkdir -p /run/sshd /var/empty /var/log
rm -f /var/log/wifi.log /var/log/akuma-sshd.log
# wifi: keep the best known network (/etc/wifi) joined; restart if it exits
( while :; do echo "--- wifi auto start $(date +%T)" >> /var/log/wifi.log
  /usr/local/bin/wifi auto >> /var/log/wifi.log 2>&1
  echo "--- wifi auto exited $? $(date +%T)" >> /var/log/wifi.log; sleep 5; done ) &
# snapshots every 15 s: kernel log, interfaces, sockets (overwritten each time)
( while :; do { date; dmesg; } > /var/log/dmesg.txt 2>&1
  { date; ifconfig -a; cat /proc/net/tcp; } > /var/log/net.txt 2>&1; sleep 15; done ) &
echo "=== openrc sysinit $(date)"
/sbin/openrc sysinit
echo "=== openrc boot $(date)"
/sbin/openrc boot
echo "=== openrc default $(date)"
/sbin/openrc default
echo "=== hostname: $(hostname) / $(uname -a)"
# Akuma's own sshd on 2222: a way in independent of OpenSSH (4444)
( while :; do /usr/local/bin/akuma-sshd >> /var/log/akuma-sshd.log 2>&1; sleep 2; done ) &
pidof sshd >/dev/null || { echo "=== sshd not up via openrc; starting directly"; /usr/sbin/sshd; }
echo "=== done $(date); idling"
# Console shell on the local framebuffer + keyboard (stdin is still the keyboard).
# Restarted if it exits; its output is the screen, not the log.
( while :; do TERM=xterm-256color HOME=/root /usr/bin/bash -l <&0 >&3 2>&4; sleep 1; done ) &
# No address after 2 minutes (the radio sometimes never joins): with /autoreboot
# present, go back to Pop so the caller can re-arm and try again.
n=0
while [ $n -lt 24 ]; do ifconfig -a 2>/dev/null | grep -q 'inet 192\.168\.' && break; sleep 5; n=$((n+1)); done
if [ $n -ge 24 ] && [ -e /autoreboot ]; then echo "=== no network after 120s"; sync; /sbin/reboot -f; fi
echo "=== network up $(date)"
while :; do sleep 3600; done
