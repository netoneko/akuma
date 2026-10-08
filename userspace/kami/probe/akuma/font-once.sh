#!/bin/sh
# Headless Chromium screenshot of a page that uses ONLY system fonts
# (serif, sans-serif, monospace, an emoji line), to find why system-font text
# does not paint on Akuma (2026-10-08, ryzen). Pair with KARGS=strace_err.
echo "== font-once: $(uname -a)"
mkdir -p /tmp
cat > /tmp/page.html <<'HTML'
<!doctype html><body style="margin:10px;background:#fff;font-size:26px">
<div style="font-family:serif">serif: The quick brown fox 0123</div>
<div style="font-family:sans-serif">sans-serif: The quick brown fox 0123</div>
<div style="font-family:monospace">monospace: The quick brown fox 0123</div>
<div style="font-family:Helvetica,Arial">Helvetica/Arial: The quick brown fox</div>
<div>emoji: 😀 🎉 ✓ → ★</div>
</body>
HTML
/usr/lib/chromium/chromium --headless --no-sandbox --disable-gpu --disable-dev-shm-usage \
  --disable-breakpad --disable-crash-reporter --no-first-run --user-data-dir=/tmp/prof \
  --enable-logging=stderr --v=1 --window-size=900,300 \
  --screenshot=/tmp/shot.png file:///tmp/page.html > /tmp/chrome.log 2>&1
echo "== chromium exit status $?"
grep -a -E "FontDataService|font" /tmp/chrome.log | cut -c1-200 | head -20
ls -la /tmp/shot.png
dmesg > /tmp/dmesg.txt 2>&1
sync
echo "== font-once done"
