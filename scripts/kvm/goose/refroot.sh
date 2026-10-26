#!/bin/sh
# refroot.sh - build the Linux reference root with the SAME apk.static the
# guest image carries (stage-8 M0, plan §2.4 / TAOXICHUAN_STAGE6_PLAN §7).
#
# Uses /root/dev-cycle/taoxichuan-s8/apk.static.guest (extracted read-only
# from /boot/akuma-dev-root-apk.img.orig, apk-tools 3.0.8-r0 static-PIE;
# re-extract with /root/dev-cycle/taoxichuan-s7/extract_apkstatic.py) against
# the frozen LAN mirror, signed index, host keys. Prints the five reference
# values the supervisor compares guest-vs-refroot after each boot:
#   1. sha256 of APKINDEX.tar.gz
#   2. /etc/apk/world line-set
#   3. apk info -L curl (file list)
#   4. sha256 of /usr/bin/curl
#   5. sha256 of every shared library curl needs (NEEDED, resolved in-root)
#
# apk-tools 3 appends $arch to a repository line, so the line carries the
# /main prefix and the wire request is /main/x86_64/APKINDEX.tar.gz - the
# same bytes the guest will ask from 192.168.1.159:8123/main.
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env

REPO=/src/github.com/netoneko/akuma
MIRROR=/srv/alpine-mirror
REF=/tmp/refroot
APKSTATIC=/root/dev-cycle/taoxichuan-s8/apk.static.guest
KEYS=/etc/apk/keys            # host keys = same official alpine RSA pubs the image ships

[ -f "$APKSTATIC" ] || { echo "no $APKSTATIC"; exit 1; }
"$REPO/scripts/kvm/goose/mirror.sh" start >/dev/null || { echo "mirror would not start"; exit 1; }

rm -rf "$REF"
echo 'http://127.0.0.1:8123/main' > /tmp/refroot.repos
"$APKSTATIC" --no-log --root "$REF" --initdb --keys-dir "$KEYS" \
  --repositories-file /tmp/refroot.repos add curl > /tmp/refroot.log 2>&1
rc=$?
if [ $rc -ne 0 ]; then tail -5 /tmp/refroot.log; echo "REFROOT FAIL apk add rc=$rc"; exit 1; fi

echo "== APKINDEX.tar.gz sha256"
sha256sum "$MIRROR/main/x86_64/APKINDEX.tar.gz"
echo "== /etc/apk/world"
sort "$REF/etc/apk/world"
echo "== apk info -L curl"
"$APKSTATIC" --no-log --root "$REF" info -L curl
echo "== /usr/bin/curl sha256"
sha256sum "$REF/usr/bin/curl"
echo "== shared libraries (NEEDED, resolved in the refroot)"
needs=$(readelf -d "$REF/usr/bin/curl" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')
echo "$needs" | while read -r lib; do
  found=""
  for d in "$REF/lib" "$REF/usr/lib"; do
    if [ -e "$d/$lib" ]; then found="$d/$lib"; break; fi
    # musl: libc.musl-x86_64.so.1 is a symlink to the loader
    if [ "$lib" = "libc.musl-x86_64.so.1" ] && [ -e "$d/ld-musl-x86_64.so.1" ]; then
      found="$d/ld-musl-x86_64.so.1"; break
    fi
  done
  if [ -n "$found" ]; then sha256sum "$found"; else echo "MISSING $lib"; fi
done
echo "REFROOT OK"
