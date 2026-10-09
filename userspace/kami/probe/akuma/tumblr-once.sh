#!/bin/sh
# In-guest: Chromium 152 against tumblr with kami's flags, under Firecracker.
# Pair with KARGS=strace_err. Prints the "== tumblr:" lines, Chromium's crash
# lines, and leaves /tmp/dmesg.txt + /tmp/shot.png for the runner to dump.
echo "== tumblr-once: $(uname -a)"
[ -f /guest.env ] && . /guest.env
mkdir -p /tmp; rm -f /tmp/shot.png /tmp/chromium.stderr
sleep ${NETWAIT:-6}
URL=${URL:-https://www.tumblr.com/}
export CHROME_EXTRA="--disable-gpu-compositing --disable-software-rasterizer --disable-background-networking --disable-sync --disable-extensions --disable-component-update --disable-default-apps --no-default-browser-check --disable-client-side-phishing-detection --disable-domain-reliability --disable-breakpad --disable-crash-reporter --disable-site-isolation-trials --renderer-process-limit=4 --disable-features=Translate,MediaRouter,OptimizationHints,BackForwardCache,AcceptCHFrame,InterestFeedContentSuggestions,IsolateOrigins,site-per-process --enable-logging=stderr --v=0"
export DBUS_SESSION_BUS_ADDRESS=disabled: DBUS_SYSTEM_BUS_ADDRESS=disabled:
python3 /tumblr.py "$URL" ${DUR:-60} 2>&1
echo "== processes alive at the end"; for d in /proc/[0-9]*; do printf "%s %s\n" "${d#/proc/}" "$(tr "\0" " " < $d/cmdline 2>/dev/null | cut -c1-90)"; done | grep -v "^[0-9]* $" | head -40
echo "== chromium stderr: crash / fatal lines"
grep -a -E 'crashed|FATAL|Check failed|Received signal|terminated' /tmp/chromium.stderr | cut -c1-200 | head -20
echo "== chromium stderr: error sites"
grep -a -o -E ':(ERROR|FATAL):[^]]+\]' /tmp/chromium.stderr | sort | uniq -c | sort -rn | head -12
dmesg > /tmp/dmesg.txt 2>&1
sync
echo "== tumblr-once done"
/bin/busybox poweroff -f
