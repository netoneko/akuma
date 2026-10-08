#!/bin/sh
# font-once.sh's page, asked about through CDP. Run: sh run-fc.sh font-trace-once.sh fonttrace.py cdp.py
echo "== font-trace-once: $(uname -a)"
mkdir -p /tmp
cat > /tmp/page.html <<'HTML'
<!doctype html><body style="margin:10px;background:#fff;font-size:26px">
<div style="font-family:serif">serif: The quick brown fox 0123</div>
<div style="font-family:sans-serif">sans-serif: The quick brown fox 0123</div>
<div style="font-family:monospace">monospace: The quick brown fox 0123</div>
<div style="font-family:Helvetica,Arial">Helvetica/Arial: The quick brown fox</div>
</body>
HTML
python3 /fonttrace.py 2>&1 | tail -20
dmesg > /tmp/dmesg.txt 2>&1
sync
echo "== font-trace-once done"
