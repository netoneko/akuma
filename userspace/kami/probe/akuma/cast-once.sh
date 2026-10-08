#!/bin/sh
# kami's CDP screencast path under Akuma, plain and with the opaque background
# override. Pair with push.sh; run: MEM=4096 sh run-fc.sh cast-once.sh castprobe.py cdp.py
echo "== cast-once: $(uname -a)"
mkdir -p /tmp
cat > /tmp/page.html <<'HTML'
<!doctype html><html><body style="font:28px sans-serif;background:#123;color:#eee;margin:40px">
<h1>kami on Akuma</h1><p id=js>JavaScript did not run</p>
<script>document.getElementById('js').textContent = 'JavaScript ran: 6 x 7 = ' + (6*7);</script>
</body></html>
HTML
python3 --version 2>&1 | sed 's/^/== cast: /'
# One run per compositor/GPU flag set: the screencast frame was fully
# transparent with the defaults (2026-10-08).
for extra in "" "--in-process-gpu" "--disable-gpu-compositing" "--use-angle=swiftshader --enable-unsafe-swiftshader" "--disable-features=VizDisplayCompositor"; do
  CHROME_EXTRA="$extra" python3 /castprobe.py plain 2>&1 | tail -12
done
dmesg > /tmp/dmesg.txt 2>&1
sync
echo "== cast-once done"
