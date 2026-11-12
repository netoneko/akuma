# Settings every script in this overlay sources. Specific to THIS machine (akuma-ryzen);
# generalize later. Override from the environment, never by editing a script.
. /etc/akuma-dev.env
export TERM=dumb GIT_PAGER=cat

# The second partition used as the guest's root: N in /dev/nvme0n1pN. NO DEFAULT — a wrong
# number is a destroyed filesystem, so every script that needs it refuses without it.
# Never p3 (the host's root; the hypervisor refuses it too), never an ESP (p1, p6).
PART=${PART:-}

OV=${OV:-$AKUMA_SRC/overlays/ryzen-akuma-dev}
VM=vm-dev-part                          # herd service name (and /var/run/herd/<VM>.vm)
GUEST_KERNEL=${GUEST_KERNEL:-/boot/akuma-guest-dev}   # NEVER /boot/akuma-amd64*: that is the host's
KBUILD_ELF=${KBUILD_ELF:-/root/ktarget/x86_64-unknown-none/release/akuma}
SSH_PORT=${SSH_PORT:-2224}              # host port forwarded to the guest's sshd (22)
OUT=${OUT:-/root/dev-cycle/ryzen-akuma-dev}
# One kvm0 VM at a time: every other VM service is stopped before ours starts.
OTHER_VMS="vm-dev vm-serve vm-serve-mc vm-serve2 vm-gentoo vm-gentoo-mc vm-netbsd"
mkdir -p "$OUT"

need_part() {
    case "$PART" in
        ''|*[!0-9]*) echo "PART=<N> is required (the guest's root is /dev/nvme0n1pN); see README.md" >&2; exit 2 ;;
        1|3|6) echo "PART=$PART refused: p1/p6 are ESPs, p3 is the host's root" >&2; exit 2 ;;
    esac
}
# The host's ssh is Akuma's own client: no -o flags (it reads them as a hostname), pubkey auth only.
gssh() { /bin/ssh -p "$SSH_PORT" root@localhost "$@"; }
