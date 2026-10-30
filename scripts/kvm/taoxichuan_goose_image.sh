#!/bin/bash
# Build /root/tx-side/goose.img: a minimal ext2 root for Taoxichuan with sshd, busybox (+ applet
# symlinks), goose and a GLM goose config. Files come from the Akuma root via `debugfs -c` (read-only,
# never mounted). The z.ai token arrives on stdin and goes straight into a 0600 secrets.yaml:
#   ssh root@<pop> /root/tx-side/taoxichuan_goose_image.sh < ~/.akuma/z.ai/token
# Boot it with: SIDE_DISK=/root/tx-side/goose.img scripts/kvm/taoxichuan_pop_side.sh boot fresh
# Then: ssh -p 2234 root@<pop> '. /etc/goose.env; goose run --with-builtin developer -t "..."'
# Settings mirror scripts/kvm/goose/goose-glm-mac.sh (z.ai, glm-5.3-flash, low thinking effort).
# 2026-10-30: goose does not start on Taoxichuan yet - tokio's runtime needs epoll_create1 (291),
# and Taoxichuan serves no epoll call.
set -euo pipefail
D=/dev/nvme0n1p3; W=/root/tx-side/rootfs; OUT=/root/tx-side/goose.img
dump() { mkdir -p "$W/$(dirname "$1")"; debugfs -c -R "dump $1 $W$1" "$D" 2>/dev/null; [ -s "$W$1" ] || { echo "missing $1" >&2; exit 1; }; }
mkdir -p "$W"/{bin,lib,usr/bin,usr/local/bin,etc/ssl/certs,etc/sshd,root/.config/goose,tmp,dev,proc,var/tmp,home}
chmod 1777 "$W/tmp" "$W/var/tmp"
[ -s "$W/usr/local/bin/goose" ] || dump /usr/local/bin/goose
[ -s "$W/bin/busybox" ] || dump /bin/busybox
[ -s "$W/bin/sshd" ] || dump /bin/sshd
dump /lib/ld-musl-x86_64.so.1
ln -sf ld-musl-x86_64.so.1 "$W/lib/libc.musl-x86_64.so.1"
for f in /etc/ssl/certs/ca-certificates.crt /etc/resolv.conf /etc/sshd/authorized_keys /etc/sshd/sshd.conf /etc/sshd/id_ed25519 /etc/sshd/id_ed25519.pub; do dump "$f"; done
chmod 600 "$W/etc/sshd/id_ed25519"
chmod 755 "$W/bin/sshd" "$W/bin/busybox" "$W/usr/local/bin/goose" "$W/lib/ld-musl-x86_64.so.1"
n=0
for a in $("$W/lib/ld-musl-x86_64.so.1" "$W/bin/busybox" --list); do
    [ -e "$W/bin/$a" ] || { ln -s busybox "$W/bin/$a"; n=$((n+1)); }
done
echo "busybox applet symlinks: $n"
printf '127.0.0.1 localhost\n' > "$W/etc/hosts"
printf 'root:x:0:0:root:/root:/bin/sh\n' > "$W/etc/passwd"
printf 'root:x:0:\n' > "$W/etc/group"
cat > "$W/root/.config/goose/config.yaml" <<'YAML'
GOOSE_TELEMETRY_ENABLED: false
GOOSE_MODE: auto
GOOSE_PROVIDER: openai
GOOSE_MODEL: glm-5.3-flash
GOOSE_THINKING_EFFORT: low
OPENAI_HOST: https://api.z.ai
OPENAI_BASE_PATH: api/coding/paas/v4/chat/completions
YAML
umask 077; printf 'OPENAI_API_KEY: %s\n' "$(cat)" > "$W/root/.config/goose/secrets.yaml"; umask 022
printf 'export HOME=/root GOOSE_DISABLE_KEYRING=1 PATH=/usr/local/bin:/bin:/usr/bin\n' > "$W/etc/goose.env"
rm -f "$OUT"
mke2fs -q -t ext2 -d "$W" -L taoxigoose "$OUT" 512M
ls -la "$OUT"; du -sh "$W"; e2fsck -fn "$OUT" >/dev/null 2>&1 && echo "fsck clean"
