#!/bin/sh
# One headless Chromium run (zygote mode), with its WHOLE stderr and exit
# status — kami-smoke.sh filters stderr to FATAL/ERROR, which hides a silent
# crash. Pair with KARGS="strace_err strace_pid=<browser pid>" and VCPUS=1 to
# get the browser's full syscall sequence (the pid is deterministic per image;
# 20 on kami-root.img as of 2026-10-08).
echo "== chrome-once: $(uname -a)"
mkdir -p /tmp
cat > /tmp/page.html <<'HTML'
<!doctype html><html><body style="font:28px sans-serif;background:#123;color:#eee;margin:40px">
<h1>kami on Akuma</h1><p id=js>JavaScript did not run</p>
<script>document.getElementById('js').textContent = 'JavaScript ran: 6 x 7 = ' + (6*7);</script>
</body></html>
HTML
/usr/lib/chromium/chromium --headless --no-sandbox --disable-gpu --disable-dev-shm-usage \
  --disable-breakpad --disable-crash-reporter --no-first-run --user-data-dir=/tmp/prof \
  --enable-logging=stderr --v=1 --window-size=800,600 \
  --screenshot=/tmp/shot.png file:///tmp/page.html > /tmp/chrome.log 2>&1
echo "== chromium exit status $?"
tail -60 /tmp/chrome.log
ls -la /tmp/shot.png
dmesg > /tmp/dmesg.txt 2>&1
sync
echo "== kami-smoke done"
