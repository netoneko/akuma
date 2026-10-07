#!/bin/sh
# The in-guest Chromium smoke test (runs as init's child under Firecracker).
# Success: /tmp/shot.png (or /tmp/shot--no-zygote.png) whose text reads
# "JavaScript ran: 6 x 7 = 42". run-fc.sh dumps both out of the image.
echo "== kami-smoke: $(uname -a)"
ls -la /usr/lib/chromium/ | grep -E 'snapshot|crashpad|chromium$|icudtl|resources.pak'
[ -x /exeprobe ] && /exeprobe
mkdir -p /tmp
cat > /tmp/page.html <<'HTML'
<!doctype html><html><body style="font:28px sans-serif;background:#123;color:#eee;margin:40px">
<h1>kami on Akuma</h1><p id=js>JavaScript did not run</p>
<script>document.getElementById('js').textContent = 'JavaScript ran: 6 x 7 = ' + (6*7);</script>
</body></html>
HTML
for mode in "" "--no-zygote"; do
  echo "== chromium start mode=[$mode]"
  /usr/lib/chromium/chromium --headless --no-sandbox $mode --disable-gpu --disable-dev-shm-usage \
    --disable-breakpad --disable-crash-reporter --no-first-run --user-data-dir=/tmp/prof$mode \
    --enable-logging=stderr --v=0 --window-size=800,600 \
    --screenshot=/tmp/shot$mode.png file:///tmp/page.html 2>&1 | grep -E 'FATAL|ERROR|written|snapshot' | head -12
  echo "== chromium mode=[$mode] done"
  ls -la /tmp/shot$mode.png
done
sync
echo "== kami-smoke done"
