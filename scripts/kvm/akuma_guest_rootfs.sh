#!/bin/sh
# Build the ext2 root image the *Akuma* KVM guest boots from (`kvmtest kvmakuma`).
#
#   scripts/kvm/akuma_guest_rootfs.sh OUT.img BUSYBOX
#
# BUSYBOX is a static x86_64 busybox (the one `amd64/mkdisk.sh` fetches:
# target/x86_64-unknown-none/release/busybox-x86_64). The guest kernel is told
# `init=/bin/busybox initargs=sh,/etc/guest.sh`; the script checks a read (md5 of
# busybox) and a write (a 256 KiB pattern file, synced), then prints
# AKUMA-GUEST-DONE, which the hypervisor takes as the pass marker. Needs
# e2fsprogs. See docs/archive/AKUMA_KVM_AKUMA_GUEST.md.
#
# Env:  NET=1        show the lease (`ifconfig`)
#       NETPROBE=1   UDP-echo probe (see the rehearsal script)
#       TCPPROBE=1   TCP-echo probe against the gateway's port 7 (`nc 10.0.2.2 7`)
#       SERVE=1      a *service* guest (`kvmserve`): sshd on port 22 and a script that never
#                    ends (no AKUMA-GUEST-DONE). Needs a built sshd (SSHD=, default
#                    userspace/target/x86_64-unknown-none/release/sshd; the amd64 build is
#                    `cargo build -p sshd --no-default-features --features akuma,emergency-paws,builtin-paws
#                    --target x86_64-unknown-none --release` in userspace/).
#       AUTHORIZED_KEYS=FILE   keys sshd accepts for root (default: target/.../amd64-ssh-test-key.pub);
#                    kvm-akuma-stage.sh adds the ryzen Akuma's own /etc/sshd/authorized_keys.
set -eu
OUT=${1:?usage: akuma_guest_rootfs.sh OUT.img BUSYBOX}
BB=${2:?usage: akuma_guest_rootfs.sh OUT.img BUSYBOX}
E2=${E2FSPROGS:-$(dirname "$(command -v mke2fs 2>/dev/null || echo /opt/homebrew/opt/e2fsprogs/sbin/mke2fs)")}
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/sbin" "$T/etc" "$T/proc" "$T/tmp"
cp "$BB" "$T/bin/busybox"
chmod 755 "$T/bin/busybox"
ln -s busybox "$T/bin/sh"
if [ -n "${SERVE:-}" ]; then
    # Enough applets that a login shell is usable. (The image cannot run busybox itself to
    # `--install`: it is an Akuma binary.)
    for a in ls cat cp mv rm mkdir rmdir ln ps kill sleep echo pwd env grep sed awk head tail wc sort uniq \
        cut tr tee touch chmod chown date uname id whoami hostname dmesg df du free uptime top vi nc wget \
        ifconfig netstat md5sum sha256sum dd sync mount umount find xargs which test true false; do
        ln -sf busybox "$T/bin/$a"
    done
    SSHD=${SSHD:-userspace/target/x86_64-unknown-none/release/sshd}
    [ -f "$SSHD" ] || { echo "no sshd at $SSHD (see the header)"; exit 1; }
    cp "$SSHD" "$T/bin/sshd"
    chmod 755 "$T/bin/sshd"
    mkdir -p "$T/etc/sshd" "$T/root"
    KEYS=${AUTHORIZED_KEYS:-target/x86_64-unknown-none/release/amd64-ssh-test-key.pub}
    [ -f "$KEYS" ] || { echo "no authorized keys at $KEYS"; exit 1; }
    cp "$KEYS" "$T/etc/sshd/authorized_keys"
    printf 'shell = /bin/sh\n' > "$T/etc/sshd/sshd.conf"
    printf 'root:x:0:0:root:/root:/bin/sh\n' > "$T/etc/passwd"
fi
cat > "$T/etc/guest.sh" <<'SH'
/bin/busybox echo AKUMA-GUEST-UP
/bin/busybox uname -a
/bin/busybox echo "guest cpus: $(/bin/busybox grep -c '^processor' /proc/cpuinfo)"
/bin/busybox md5sum /bin/busybox
/bin/busybox yes | /bin/busybox head -c 262144 > /wtest
/bin/busybox sync
/bin/busybox md5sum /wtest
SH
if [ -n "${NET:-}" ]; then
    # The NIC guest (`kvmnet`): show the lease. The hypervisor's own counters (DHCP, ARP)
    # are the verdict; these lines are for the log.
    echo '/bin/busybox ifconfig' >> "$T/etc/guest.sh"
fi
if [ -n "${NETPROBE:-}" ]; then
    # Also talk to the gateway the hypervisor plays: UDP echo on its port 7 (busybox ping
    # needs a raw socket, which Akuma has not). Its `-w 2` wait takes minutes of wall
    # time under TCG, so it is opt-in.
    echo '/bin/busybox echo "udp echo: $(/bin/busybox echo akuma-net-probe | /bin/busybox nc -u -w 2 10.0.2.2 7)"' >> "$T/etc/guest.sh"
fi
if [ -n "${TCPPROBE:-}" ]; then
    echo '/bin/busybox echo "tcp echo: $(/bin/busybox echo akuma-tcp-probe | /bin/busybox nc -w 3 10.0.2.2 7)"' >> "$T/etc/guest.sh"
fi
if [ -n "${SERVE:-}" ]; then
    cat >> "$T/etc/guest.sh" <<'SH'
/bin/busybox ifconfig
/bin/sshd --port 22 &
/bin/busybox echo AKUMA-GUEST-SSHD-STARTED
while true; do /bin/busybox sleep 3600; done
SH
else
    echo '/bin/busybox echo AKUMA-GUEST-DONE' >> "$T/etc/guest.sh"
fi
rm -f "$OUT"
"$E2/mke2fs" -q -t ext2 -d "$T" -L akumaguest "$OUT" 16M
echo "wrote $OUT"
