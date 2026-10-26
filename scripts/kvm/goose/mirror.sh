#!/bin/sh
# mirror.sh - verify and serve the frozen Alpine LAN mirror (stage-8 M0).
#
#   mirror.sh verify         sha256sum -c MANIFEST.sha256; exit 0 + "MANIFEST OK <N> files"
#   mirror.sh serve [dir]    verify first, then serve on :8123 (foreground)
#   mirror.sh start|stop     background serve (default dir /srv/alpine-mirror)
#
# The mirror is the guest's only package source (no internet, no TLS in the
# guest, ever): frozen 2026-10-26 from dl-cdn latest-stable/main x86_64, the
# exact 14-package curl closure (U2) + APKINDEX.tar.gz. MANIFEST.sha256 is the
# integrity contract - a mismatched byte aborts the serve, never a boot.
# Kept OUT of herd's enabled set: started/stopped by hand or by gate scripts.
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env

MIRROR=${2:-/srv/alpine-mirror}
BIND=0.0.0.0            # 192.168.1.159:8123 for the guest + 127.0.0.1:8123 for refroot
PORT=8123
PIDF=/tmp/alpine-mirror-8123.pid

verify() {
  [ -f "$MIRROR/MANIFEST.sha256" ] || { echo "MANIFEST MISSING $MIRROR"; return 1; }
  # Manifest paths are relative to the mirror root (cd so -c resolves there).
  ( cd "$MIRROR" && sha256sum -c MANIFEST.sha256 >/tmp/mirror-verify.log 2>&1 )
  rc=$?
  if [ $rc -ne 0 ]; then tail -5 /tmp/mirror-verify.log; echo "MANIFEST BAD"; return 1; fi
  n=$(grep -c . "$MIRROR/MANIFEST.sha256")
  echo "MANIFEST OK $n files"
  return 0
}

case "${1:-}" in
verify)
  verify ;;
serve)
  verify || exit 1
  exec python3 -m http.server "$PORT" --bind "$BIND" --directory "$MIRROR" ;;
start)
  verify || exit 1
  if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
    echo "mirror already running pid $(cat "$PIDF")"; exit 0
  fi
  python3 -m http.server "$PORT" --bind "$BIND" --directory "$MIRROR" \
    >/tmp/alpine-mirror-8123.log 2>&1 &
  echo $! > "$PIDF"
  sleep 1
  kill -0 "$(cat "$PIDF")" 2>/dev/null && echo "MIRROR UP pid $(cat "$PIDF")" || { echo "MIRROR FAIL"; exit 1; }
  ;;
stop)
  if [ -f "$PIDF" ]; then
    kill "$(cat "$PIDF")" 2>/dev/null && echo "MIRROR DOWN" || echo "mirror not running"
    rm -f "$PIDF"
  else
    echo "mirror not running"
  fi
  ;;
*)
  echo "usage: mirror.sh verify|serve|start|stop [dir]"; exit 2 ;;
esac
