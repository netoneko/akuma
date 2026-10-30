#!/usr/bin/env bash
# Build Linux as a KVM guest kernel in Docker and print its audit surface with
# `scripts/cloc_akuma.py --linux` — the Linux column beside
# `scripts/cloc_akuma.py --audit`. Method and caveats:
# docs/archive/AUDIT_SURFACE_METHODOLOGY.md
#
#   scripts/linux_audit_surface.sh [firecracker|minimal|host] [VERSION]
#   scripts/linux_audit_surface.sh count [VERSION]   # re-count every built tree, + unions
#
#   firecracker  Firecracker's CI guest config for VERSION — what a production
#                microVM boots. Default.
#   minimal      tinyconfig + kvm_guest.config + only the features taoxichuan
#                itself provides (virtio-mmio blk/net, ext2, IPv4, serial,
#                ELF, pipes/futex/epoll/eventfd). The floor: Linux trimmed to
#                the same job.
#   host         x86_64_defconfig + KVM (Intel and AMD) + TUN + vhost-net, all
#                built in: a Linux KVM host. A distro host kernel is several
#                times this; defconfig is the floor for "a box that runs microVMs".
#   count        no build: re-count every tree already built for VERSION, each
#                alone and then their line-level unions (host + guest = a stack).
#
# Only `vmlinux` is built: a PVH guest boots the ELF, so the bzImage real-mode
# setup and modules are not in what runs (for `host`, modules are what a distro
# would add on top; built-in only keeps the count to what one config compiles). The tree and objects live in the
# Docker volume `akuma-linux-audit` (inside Docker's VM disk, not the host's);
# `docker volume rm akuma-linux-audit` reclaims it. A shallow clone of one tag
# is ~1.5 GB, each build ~1-3 GB.
#
# NATIVE=1 runs the same steps on this machine instead (a Linux box with gcc,
# make, flex, bison, bc, libelf, libssl, unifdef, python3, git, curl), under
# LINUX_AUDIT_DIR (default ~/linux-audit). That is how the ryzen's Pop!_OS side
# runs it: copy this script and cloc_akuma.py into one directory there, then
#   NATIVE=1 ./linux_audit_surface.sh minimal 6.18
# On an x86_64 host no cross compiler is involved. JOBS=N caps make's -j (default
# nproc) for a box that is also running guests.
set -euo pipefail

MODE=${1:-firecracker}
VER=${2:-6.18}
case "$MODE" in firecracker|minimal|host|count) ;; *) echo "usage: $0 [firecracker|minimal|host|count] [VERSION]" >&2; exit 2;; esac
REPO=$(cd "$(dirname "$0")/.." && pwd)
export MODE VER

# The script is passed with `bash -c`, not on stdin: `bash -s` shares stdin with
# every child, and apt's debconf reads stdin — it can eat the rest of the script.
INNER=$(cat <<'EOF'
set -euo pipefail
if [ "${IN_DOCKER:-0}" = 1 ]; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq </dev/null
    apt-get install -y -qq --no-install-recommends gcc libc6-dev gcc-x86-64-linux-gnu make flex bison bc \
        libelf-dev libssl-dev python3 git ca-certificates unifdef perl curl cpio </dev/null >/dev/null
fi
for t in gcc make flex bison bc unifdef python3 git curl; do
    command -v "$t" >/dev/null || { echo "missing tool: $t" >&2; exit 1; }
done
CROSS=
[ "$(uname -m)" = x86_64 ] || CROSS=x86_64-linux-gnu-

SRC=$W/linux-$VER
O=$W/out-$VER-$MODE
# Clone beside, rename when complete: an interrupted clone must not pass for a tree.
if [ ! -d "$SRC" ]; then
    rm -rf "$SRC.partial"
    git clone -q --depth 1 --branch "v$VER" https://github.com/torvalds/linux "$SRC.partial"
    mv "$SRC.partial" "$SRC"
fi
if [ "$MODE" = count ]; then
    # Every tree already configured and built for this version (it has an
    # autoconf.h): each alone, then their unions.
    trees=$(for d in "$W"/out-"$VER"-*; do [ -f "$d/include/generated/autoconf.h" ] && echo "$d"; done | sort)
    exec python3 "$S/cloc_akuma.py" --linux "$SRC" $trees --top 25
fi
mkdir -p "$O"
mk() { make -s -C "$SRC" O="$O" ARCH=x86_64 CROSS_COMPILE="$CROSS" "$@"; }

if [ "$MODE" = host ]; then
    mk x86_64_defconfig
    cat > "$O/kvm-host.config" <<'FRAG'
CONFIG_VIRTUALIZATION=y
CONFIG_KVM=y
CONFIG_KVM_INTEL=y
CONFIG_KVM_AMD=y
CONFIG_TUN=y
CONFIG_VHOST_NET=y
FRAG
    "$SRC/scripts/kconfig/merge_config.sh" -m -O "$O" "$O/.config" "$O/kvm-host.config" >/dev/null
    mk olddefconfig
    while read -r want; do
        grep -qx "$want" "$O/.config" || echo "warning: $want did not survive olddefconfig" >&2
    done < "$O/kvm-host.config"
elif [ "$MODE" = firecracker ]; then
    curl -fsSL -o "$O/.config" \
        "https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-x86_64-$VER.config"
    mk olddefconfig
else
    cat > "$O/taoxichuan-parity.config" <<'FRAG'
CONFIG_64BIT=y
CONFIG_PRINTK=y
CONFIG_TTY=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_BINFMT_ELF=y
CONFIG_BINFMT_SCRIPT=y
CONFIG_BLOCK=y
CONFIG_BLK_DEV=y
CONFIG_VIRTIO_MENU=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES=y
CONFIG_VIRTIO_BLK=y
CONFIG_NET=y
CONFIG_INET=y
CONFIG_UNIX=y
CONFIG_NETDEVICES=y
CONFIG_NET_CORE=y
CONFIG_VIRTIO_NET=y
CONFIG_EXT2_FS=y
CONFIG_FUTEX=y
CONFIG_EPOLL=y
CONFIG_EVENTFD=y
CONFIG_MEMBARRIER=y
CONFIG_MULTIUSER=y
CONFIG_PVH=y
FRAG
    mk tinyconfig
    mk kvm_guest.config
    "$SRC/scripts/kconfig/merge_config.sh" -m -O "$O" "$O/.config" "$O/taoxichuan-parity.config" >/dev/null
    mk olddefconfig
    # A fragment line Kconfig rejected (unmet dependency) is silently dropped: say so.
    while read -r want; do
        grep -qx "$want" "$O/.config" || echo "warning: $want did not survive olddefconfig" >&2
    done < "$O/taoxichuan-parity.config"
fi

JOBS=${JOBS:-$(nproc)}
echo "building vmlinux ($MODE, v$VER), -j$JOBS..." >&2
mk -j"$JOBS" vmlinux
python3 "$S/cloc_akuma.py" --linux "$SRC" "$O" --top 25
EOF
)
if [ "${NATIVE:-0}" = 1 ]; then
    W=${LINUX_AUDIT_DIR:-$HOME/linux-audit}
    mkdir -p "$W"
    MODE="$MODE" VER="$VER" W="$W" S="$(cd "$(dirname "$0")" && pwd)" JOBS="${JOBS:-}" bash -c "$INNER"
else
    docker run --rm -v akuma-linux-audit:/w -v "$REPO/scripts":/s:ro -e IN_DOCKER=1 \
        -e MODE="$MODE" -e VER="$VER" -e W=/w -e S=/s debian:trixie bash -c "$INNER"
fi
