#!/bin/sh
# install.sh — from a URL to an ssh prompt on Akuma (amd64, Taoxichuan).
#
# Written to proposals/INSTALL_SCRIPT.md. POSIX sh, no bashisms, no python, no
# sudo, no package installs, no Docker. The reader is an agent: one line per
# stage, fixed exit codes, machine-readable AKUMA_* closing lines.
#
#   sh install.sh up                        # clone, build, image, boot, wait for ssh
#   sh install.sh up --vmm firecracker      # boot through Firecracker instead of QEMU
#   sh install.sh up --dry-run              # print every step, run none
#   sh install.sh check                     # preflight: what is missing and how to get it
#   sh install.sh ssh 'busybox uname -a'    # run a command in the guest
#   sh install.sh status | down             # is it up / stop the VM this script started
#
# Exit codes: 0 ok, 2 bad arguments, 10 a host dependency is missing,
# 20 clone, 30 build, 40 image, 50 boot, 60 the guest never answered ssh,
# 70 toolchain staging. Every stage logs to $DIR/logs/<stage>.log; on failure
# the script prints that path and its last 20 lines.
#
# Downloads are pinned by sha256 in scripts/install/lock.txt; a changed
# upstream file is a loud failure, never a different image.
#
# I1 scope (proposal § Steps): x86_64 hosts, kernel `taoxichuan`,
# `--vmm qemu` (default) or `--vmm firecracker`. arm64 and `--with-rust`
# are I4/I2, not here. Boot patterns follow the repo's own launchers:
# QEMU microvm line from scripts/kvm/taoxichuan/tcg_rig.py, Firecracker
# config/launch from overlays/devbox-firecracker/run.sh and
# scripts/benchmarks/ryzen_fc/fcrun.sh. Readiness is an ssh round-trip.

set -u

# ---------------------------------------------------------------- configuration

SCRIPT_PATH=$(cd "$(dirname "$0")" && pwd)/$(basename "$0")
DIR="$HOME/akuma"
PORT=2222
PORT_SET=0
VMM=qemu
REF=post-release-improvements
REPO=https://github.com/netoneko/akuma
ARCH=x86_64
KERNEL_NAME=taoxichuan
KEY_PUB=
KERNEL_OVERRIDE=       # --kernel-bin PATH: boot this kernel, do not build one
SSHD_OVERRIDE=         # --sshd-bin PATH: put this sshd in the image, do not build one
VMM_SET=0
PERSIST=0
DRY_RUN=0
CLEAN=0
MEM_MIB=1024
SMP=1
WAIT_DEADLINE=180      # TCG: 600
CMD=up

usage() {
	sed -n '2,19p' "$SCRIPT_PATH"
}

# ---------------------------------------------------------------------- helpers

fatal() {                                     # fatal <exit-code> <stage> <message...>
	code=$1; stage=$2; shift 2
	printf 'install.sh: FAIL stage=%s %s\n' "$stage" "$*"
	if [ "$DRY_RUN" = 0 ] && [ -f "$DIR/logs/$stage.log" ]; then
		printf 'install.sh: log %s, last 20 lines:\n' "$DIR/logs/$stage.log"
		tail -20 "$DIR/logs/$stage.log"
	fi
	exit "$code"
}

note() { printf 'install.sh: %s\n' "$*"; }

# run — execute a command, or just print it under --dry-run.
run() {
	if [ "$DRY_RUN" = 1 ]; then
		printf 'DRY: %s\n' "$*"
		return 0
	fi
	"$@"
}

stamp()   { [ "$DRY_RUN" = 1 ] && return 0; mkdir -p "$DIR/state"; : >"$DIR/state/$1.stamp"; }
stamped() { [ -f "$DIR/state/$1.stamp" ]; }

deps_ok=0
missing=''                                    # lines of "tool|install command"
missing_tools=''                              # just the tool names, for the concrete fix line

need() {                                      # need <tool> <install-command...>
	if command -v "$1" >/dev/null 2>&1; then return 0; fi
	deps_ok=1
	missing="$missing$1|$2
"
	missing_tools="$missing_tools $1"
}

# ------------------------------------------------------------------- arg parsing

if [ $# -lt 1 ]; then usage; exit 2; fi
CMD=$1; shift
case "$CMD" in
	up|check|ssh|status|down) ;;
	*) usage; exit 2 ;;
esac

while [ $# -gt 0 ]; do
	case "$1" in
		--dir)     DIR=$2; shift 2 ;;
		--port)    PORT=$2; PORT_SET=1; shift 2 ;;
		--vmm)     VMM=$2; VMM_SET=1; shift 2 ;;
		--ref)     REF=$2; shift 2 ;;
		--arch)    ARCH=$2; shift 2 ;;
		--kernel)  KERNEL_NAME=$2; shift 2 ;;
		--key)     KEY_PUB=$2; shift 2 ;;
		--kernel-bin) KERNEL_OVERRIDE=$2; shift 2 ;;
		--sshd-bin) SSHD_OVERRIDE=$2; shift 2 ;;
		--mem)     MEM_MIB=$2; shift 2 ;;
		--smp)     SMP=$2; shift 2 ;;
		--persist) PERSIST=1; shift ;;
		--dry-run) DRY_RUN=1; shift ;;
		--clean)   CLEAN=1; shift ;;
		-h|--help) usage; exit 0 ;;
		--)        shift; break ;;
		*)         if [ "$CMD" = ssh ]; then break; fi   # the rest is the command to run in the guest
		           printf 'install.sh: unknown argument %s\n' "$1" >&2; usage; exit 2 ;;
	esac
done

case "$ARCH" in x86_64|amd64) ARCH=x86_64 ;; *) printf 'install.sh: only x86_64 in I1 (arm64 is I4)\n' >&2; exit 2 ;; esac
case "$KERNEL_NAME" in taoxichuan) ;; *) printf 'install.sh: only --kernel taoxichuan in I1\n' >&2; exit 2 ;; esac
case "$VMM" in qemu|firecracker|akuma) ;; *) printf 'install.sh: --vmm qemu|firecracker|akuma\n' >&2; exit 2 ;; esac
case "$PORT" in ''|*[!0-9]*) printf 'install.sh: --port needs a number\n' >&2; exit 2 ;; esac
if [ "$CMD" = ssh ]; then [ $# -ge 1 ] || { printf 'install.sh: ssh needs a command\n' >&2; exit 2; }; fi

SRC="$DIR/src"
STATE="$DIR/state"
LOGS="$DIR/logs"
CACHE="$DIR/cache"
TARGET=x86_64-unknown-none
KERNEL_BIN="$SRC/target/$TARGET/release/taoxichuan"
SSHD_BIN="$SRC/userspace/target/$TARGET/release/sshd"
IMG="$STATE/disk.img"
if [ -n "$KERNEL_OVERRIDE" ]; then
	[ -f "$KERNEL_OVERRIDE" ] || { printf 'install.sh: --kernel-bin %s: no such file\n' "$KERNEL_OVERRIDE" >&2; exit 2; }
	KERNEL_BIN=$KERNEL_OVERRIDE
fi
if [ -n "$SSHD_OVERRIDE" ]; then
	[ -f "$SSHD_OVERRIDE" ] || { printf 'install.sh: --sshd-bin %s: no such file\n' "$SSHD_OVERRIDE" >&2; exit 2; }
	SSHD_BIN=$SSHD_OVERRIDE
fi

if [ "$DRY_RUN" = 0 ]; then
	mkdir -p "$STATE" "$LOGS" "$CACHE" || exit 2
	if [ "$CLEAN" = 1 ]; then
		note "--clean: removing $STATE and $CACHE (logs kept)"
		rm -rf "$STATE" "$CACHE"
		mkdir -p "$STATE" "$CACHE"
	fi
fi

# SSH options: no host-key prompts (the guest makes a fresh host key on first
# boot), short connect timeout. With --key the private half stays with the
# user, so no -i and ssh falls back to the default identities.
if [ "$VMM_SET" = 0 ] && [ -f "$STATE/vmm" ]; then VMM=$(cat "$STATE/vmm"); fi   # what 'up' booted
GUEST_HOST=127.0.0.1                          # qemu: hostfwd on loopback
# on Akuma the host's own sshd owns 2222, so a forward there would reach the host, not the VM
[ "$VMM" = akuma ] && [ "$PORT_SET" = 0 ] && PORT=2230
SSHPORT=$PORT                                 # qemu: the hostfwd port; firecracker: the guest's own 22
[ "$VMM" = firecracker ] && SSHPORT=22
if [ "$VMM" = firecracker ]; then GUEST_HOST=10.0.2.15; fi   # firecracker: the guest on its tap
KEYOPT="-i $STATE/key"
# Which ssh is this? OpenSSH takes -o options; Akuma's own client does not (it reads "-o" as a host name), so it is called
# with only -i and -p. Ask with -h: OpenSSH prints usage and exits, Akuma's client prints usage naming itself. Never ask
# with -V: Akuma's client would take "-V" for a host and try to connect to it.
SSH_FLAVOR=openssh
if command -v ssh >/dev/null 2>&1; then
	case "$(ssh -h 2>&1 </dev/null)" in *Akuma*) SSH_FLAVOR=akuma ;; esac
fi
SSH_EXTRA="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes"
[ "$SSH_FLAVOR" = akuma ] && SSH_EXTRA=""
# Akuma ssh has no StrictHostKeyChecking: it asks yes/no on stdin the first time.
# Feed it "yes" (the VM is one we just booted on loopback; the key lands in known_hosts).
SSH_YES=
# a fresh VM has a fresh host key behind the same host:port; Akuma's client refuses a changed key
forget_host() { [ "$SSH_FLAVOR" = akuma ] && [ -f "$HOME/.ssh/known_hosts" ] && sed -i "/^$GUEST_HOST:$SSHPORT /d" "$HOME/.ssh/known_hosts"; return 0; }
[ "$SSH_FLAVOR" = akuma ] && SSH_YES="echo yes |"
SSH_OPTS="$KEYOPT -p $SSHPORT $SSH_EXTRA"

# -----------------------------------------------------------------------: check

if [ "$CMD" = check ]; then
	[ -n "$KERNEL_OVERRIDE" ] && [ -n "$SSHD_OVERRIDE" ] || need git      "git: apt install git / apk add git / brew install git"
	need curl     "curl: apt install curl / apk add curl / brew install curl"
	[ -n "$KERNEL_OVERRIDE" ] && [ -n "$SSHD_OVERRIDE" ] || need cargo    "rustup: curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain nightly"
	need mke2fs   "e2fsprogs: apt install e2fsprogs / apk add e2fsprogs / brew install e2fsprogs"
	need ssh      "openssh: apt install openssh-client / apk add openssh-client / brew install openssh-client"
	[ -n "$KEY_PUB" ] || need ssh-keygen "openssh (ssh-keygen): as ssh; on Akuma it is the 'ssh' binary under another name (ln -s ssh /bin/ssh-keygen; needs the ssh from userspace/sshd, see its README); or rerun with --key /path/to/key.pub"
	if [ "$VMM" = qemu ]; then
		need qemu-system-x86_64 "qemu: apt install qemu-system-x86 / apk add qemu-system-x86_64 / brew install qemu"
	elif [ "$VMM" = akuma ]; then
		need herd "herd: --vmm akuma only works on an Akuma host (Akuma's own hypervisor, started through herd)"
	else
		need firecracker "firecracker: download the release binary from https://github.com/firecracker-microvm/firecracker/releases (needs /dev/kvm)"
	fi
	if [ "$VMM" = firecracker ] && [ ! -w /dev/kvm ]; then
		deps_ok=1
		missing="$missing/dev/kvm|not writable by $(id -un): an administrator runs once  sudo usermod -aG kvm $(id -un)  then log in again (or sudo setfacl -m u:$(id -un):rw /dev/kvm until reboot)
"
		missing_tools="$missing_tools"
	fi
	debugfs_ok=no
	command -v debugfs >/dev/null 2>&1 && debugfs_ok=yes
	kvm_ok=no
	[ -w /dev/kvm ] && kvm_ok=yes
	mke2fs_d=unknown
	if command -v mke2fs >/dev/null 2>&1; then
		probe="$CACHE/probe.img"
		if mke2fs -q -F -t ext2 -b 1024 -d "$CACHE" "$probe" 1024 >/dev/null 2>&1; then
			mke2fs_d=yes
		else
			mke2fs_d=no
		fi
		rm -f "$probe"
	fi
	diskfree=$(df -Pk "$DIR" 2>/dev/null | awk 'NR==2{print $4}')
	disknote="need at least 2097152 KiB"
	case "$diskfree" in
		''|0|*[!0-9]*)
			# df can lie (sandboxes, odd overlays); prove writability instead
			if dd if=/dev/zero of="$CACHE/.wtest" bs=1048576 count=8 2>/dev/null; then
				disknote="df looks wrong here; 8 MiB write test: ok"
			else
				disknote="df looks wrong here; 8 MiB write test: FAILED"
			fi
			rm -f "$CACHE/.wtest"
			;;
	esac
	echo "disk free at $DIR: ${diskfree:-?} KiB ($disknote)"
	{
		echo "git:        $(command -v git || echo MISSING)"
		echo "curl:       $(command -v curl || echo MISSING)"
		if [ -n "$KERNEL_OVERRIDE" ] && [ -n "$SSHD_OVERRIDE" ]; then echo "cargo:      not needed (kernel and sshd supplied)"; else echo "cargo:      $(command -v cargo || echo MISSING)"; fi
		echo "mke2fs:     $(command -v mke2fs || echo MISSING) (-d: $mke2fs_d)"
		echo "debugfs:    $(command -v debugfs || echo MISSING)"
		echo "ssh:        $(command -v ssh || echo MISSING)"
		echo "ssh-keygen: $(command -v ssh-keygen || echo MISSING)$([ -n "$KEY_PUB" ] && echo ' (--key given: not needed)')"
		if [ "$VMM" = qemu ]; then echo "qemu-system-x86_64: $(command -v qemu-system-x86_64 || echo MISSING)"; else echo "qemu-system-x86_64: not needed for --vmm $VMM"; fi
		if [ "$VMM" = firecracker ]; then echo "firecracker: $(command -v firecracker || echo MISSING)"; else echo "firecracker: not needed for --vmm $VMM"; fi
		if [ "$VMM" = akuma ]; then echo "herd (VMs are herd services, kvm0): $(command -v herd || echo MISSING)"; else echo "/dev/kvm writable: $kvm_ok (no: TCG, slow — the wait stage gets 600 s)"; fi
		echo "disk free at $DIR: ${diskfree:-?} KiB ($disknote)"
	} | tee "$LOGS/check.log"
	if [ "$deps_ok" = 1 ]; then
		printf 'install.sh: missing host dependencies:\n'
		printf '%s\n' "$missing" | while IFS='|' read -r tool how; do
			[ -n "$tool" ] && printf '  %-18s %s\n' "$tool" "$how"
		done
		printf 'install.sh: fix each with the exact command shown above (no sudo needed if you are root), then rerun check — do not ask the user which to use\n'
		mgr=none
		command -v apt-get >/dev/null 2>&1 && mgr=apt
		command -v apk >/dev/null 2>&1 && mgr=apk
		command -v brew >/dev/null 2>&1 && mgr=brew
		pkgs=''
		for t in $missing_tools; do
			case "$mgr:$t" in
				apk:git|apt:git|brew:git)          p=git ;;
				apk:curl|apt:curl|brew:curl)       p=curl ;;
				apk:mke2fs|apt:mke2fs|brew:mke2fs) p=e2fsprogs ;;
				apk:ssh|apt:ssh|brew:ssh)          p=openssh-client ;;
				apk:ssh-keygen)                    p=openssh-keygen ;;
				apt:ssh-keygen|brew:ssh-keygen)    p=openssh-client ;;
				apk:qemu-system-x86_64)            p=qemu-system-x86_64 ;;
				apt:qemu-system-x86_64)            p=qemu-system-x86 ;;
				brew:qemu-system-x86_64)           p=qemu ;;
				*)                                 p='' ;;
			esac
			[ -n "$p" ] && pkgs="$pkgs $p"
		done
		case "$mgr" in
			apk)  [ -n "$pkgs" ] && printf 'install.sh: on this host the fastest fix is: apk add%s   # run it now, then rerun check\n' "$pkgs" ;;
			apt)  [ -n "$pkgs" ] && printf 'install.sh: on this host the fastest fix is: apt install%s   # run it now, then rerun check\n' "$pkgs" ;;
			brew) [ -n "$pkgs" ] && printf 'install.sh: on this host the fastest fix is: brew install%s   # run it now, then rerun check\n' "$pkgs" ;;
		esac
		exit 10
	fi
	if [ "$mke2fs_d" = no ] && [ "$debugfs_ok" = no ]; then
		note "mke2fs lacks -d and debugfs is absent: the image stage will create the fs and populate it through a loop mount (needs root)"
	fi
	note "check: prerequisites present (kvm=$kvm_ok vmm=$VMM mke2fs -d: $mke2fs_d)"
	exit 0
fi

# ------------------------------------------------------------------: down/status

if [ "$CMD" = down ] && [ "$VMM" = akuma ]; then
	if [ -f "$STATE/akuma.svc" ]; then
		svc=$(cat "$STATE/akuma.svc")
		herd stop "$svc" >/dev/null 2>&1 || :
		herd disable "$svc" >/dev/null 2>&1 || :
		rm -f "/etc/herd/available/$svc.conf" "/etc/herd/enabled/$svc.conf" "$STATE/akuma.svc"
		note "down: stopped herd service $svc - only what this script started"
	else
		note "down: no $STATE/akuma.svc (nothing this script started)"
	fi
	exit 0
fi

if [ "$CMD" = down ]; then
	pidfile="$STATE/$VMM.pid"
	if [ -f "$pidfile" ]; then
		pid=$(cat "$pidfile")
		if kill -0 "$pid" 2>/dev/null; then
			kill "$pid"
			i=0
			while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 10 ]; do sleep 1; i=$((i+1)); done
			kill -9 "$pid" 2>/dev/null || :
			note "down: stopped pid $pid ($VMM) — only what this script started, never pkill"
		else
			note "down: pid $pid ($VMM) already gone"
		fi
		rm -f "$pidfile"
	else
		note "down: no pidfile $pidfile (nothing this script started)"
	fi
	if [ "$VMM" = firecracker ] && [ -f "$STATE/fc.tap" ]; then
		tap=$(cat "$STATE/fc.tap")
		ip link del "$tap" 2>/dev/null || :
		rm -f "$STATE/fc.tap"
		note "down: removed tap $tap"
	fi
	rm -f "$STATE/fc.json"
	exit 0
fi

if [ "$CMD" = status ] && [ "$VMM" = akuma ]; then
	if [ -f "$STATE/akuma.svc" ] && herd status 2>/dev/null | grep -q "^ *$(cat "$STATE/akuma.svc"): running"; then
		note "status: akuma running (herd service $(cat "$STATE/akuma.svc")), ssh target root@$GUEST_HOST -p $SSHPORT"
		exit 0
	fi
	note "status: not running"
	exit 1
fi

if [ "$CMD" = status ]; then
	if [ -f "$STATE/$VMM.pid" ] && kill -0 "$(cat "$STATE/$VMM.pid")" 2>/dev/null; then
		note "status: $VMM running, pid $(cat "$STATE/$VMM.pid"), ssh target root@$GUEST_HOST -p $SSHPORT"
		exit 0
	fi
	note "status: not running"
	exit 1
fi

if [ "$CMD" = ssh ]; then
	[ -f "$STATE/key" ] || fatal 2 ssh "no $STATE/key — run up first"
	# shellcheck disable=SC2086
	if [ "$SSH_FLAVOR" = akuma ]; then echo yes | ssh $SSH_OPTS "root@$GUEST_HOST" "$@"; exit $?; fi
	exec ssh $SSH_OPTS "root@$GUEST_HOST" "$@"
fi

# -------------------------------------------------------------------------: up

# stage clone --------------------------------------------------------------
if [ -n "$KERNEL_OVERRIDE" ] && [ -n "$SSHD_OVERRIDE" ]; then
	note "clone: skipped (kernel and sshd supplied)"
elif stamped clone && [ -d "$SRC" ]; then
	note "clone: already done ($SRC)"
else
	if [ "$DRY_RUN" = 1 ]; then
		run git clone --depth 1 --branch "$REF" --no-recurse-submodules "$REPO" "$SRC"
	else
		if ! ( git clone --depth 1 --branch "$REF" --no-recurse-submodules "$REPO" "$SRC" ) >"$LOGS/clone.log" 2>&1; then
			rm -rf "$SRC"
			fatal 20 clone "git clone failed (network? does branch $REF exist?)"
		fi
		stamp clone
	fi
	note "clone: $REPO branch $REF -> $SRC"
fi

# stage build --------------------------------------------------------------
if [ -n "$KERNEL_OVERRIDE" ] && [ -n "$SSHD_OVERRIDE" ]; then
	note "build: skipped (kernel $KERNEL_OVERRIDE and sshd $SSHD_OVERRIDE supplied)"
elif stamped build && [ -f "$KERNEL_BIN" ] && [ -x "$SSHD_BIN" ]; then
	note "build: already done"
else
	if [ "$DRY_RUN" = 1 ]; then
		run sh -c "cd '$SRC' && cargo build -p taoxichuan --release --target '$TARGET' --features no-tests"
		run sh -c "cd '$SRC/userspace' && cargo build -p sshd --release --target '$TARGET' --features libakuma/linux-abi"
	else
		command -v cargo >/dev/null 2>&1 || fatal 30 build "cargo not found — the check stage prints the rustup one-liner"
		# NOTE: with rustup absent, rust-toolchain.toml is ignored and this
		# box builds with its installed nightly (2026-09-29) while the tree
		# pins 2026-09-19. Recorded in RESULT-I1.md, not hidden.
		if [ -n "$KERNEL_OVERRIDE" ]; then
			note "build: kernel skipped, booting the supplied $KERNEL_OVERRIDE"
		elif ! ( cd "$SRC" && cargo build -p taoxichuan --release --target "$TARGET" --features no-tests ) >>"$LOGS/build.log" 2>&1; then
			fatal 30 build "kernel build failed (needs rust-std for $TARGET)"
		fi
		if ! ( cd "$SRC/userspace" && cargo build -p sshd --release --target "$TARGET" --features libakuma/linux-abi ) >>"$LOGS/build.log" 2>&1; then
			fatal 30 build "sshd build failed"
		fi
		[ -f "$KERNEL_BIN" ] || fatal 30 build "kernel binary missing after build: $KERNEL_BIN"
		[ -x "$SSHD_BIN" ] || fatal 30 build "sshd binary missing after build: $SSHD_BIN"
		stamp build
	fi
	note "build: $KERNEL_BIN + $SSHD_BIN"
fi

# stage fetch --------------------------------------------------------------
# The lock file ships next to the script; the $SRC copy is the same file.
LOCK="$(dirname "$SCRIPT_PATH")/install/lock.txt"
[ -f "$LOCK" ] || LOCK="$SRC/scripts/install/lock.txt"
BB="$CACHE/busybox-$ARCH"
bb_url=$(awk -v a="$ARCH" '$3==a && $2 ~ /busybox/ {print $2; exit}' "$LOCK" 2>/dev/null)
bb_sha=$(awk -v a="$ARCH" '$3==a && $2 ~ /busybox/ {print $1; exit}' "$LOCK" 2>/dev/null)
if [ -z "$bb_url" ] || [ -z "$bb_sha" ]; then
	fatal 10 fetch "no $ARCH entry in $LOCK — the script never fetches without a hash"
fi
if stamped fetch && [ -f "$BB" ] && echo "$bb_sha  $BB" | sha256sum -c >/dev/null 2>&1; then
	note "fetch: busybox-$ARCH already pinned at $BB"
else
	if [ "$DRY_RUN" = 1 ]; then
		run curl -fsSL -o "$BB" "$bb_url"
		run sh -c "echo '$bb_sha  $BB' | sha256sum -c -"
	else
		if ! ( curl -fsSL -o "$BB" "$bb_url" ) >"$LOGS/fetch.log" 2>&1; then
			fatal 10 fetch "busybox download failed: $bb_url"
		fi
		if ! echo "$bb_sha  $BB" | sha256sum -c - >>"$LOGS/fetch.log" 2>&1; then
			fatal 10 fetch "busybox sha256 mismatch against $LOCK — upstream changed; refusing"
		fi
		chmod 755 "$BB"
		stamp fetch
	fi
	note "fetch: busybox-$ARCH sha256=${bb_sha#????????????????????????????????}.. pinned"
fi

# stage key ----------------------------------------------------------------
if [ -n "$KEY_PUB" ]; then
	[ -f "$KEY_PUB" ] || fatal 10 key "--key $KEY_PUB is not a file"
	if [ "$DRY_RUN" = 1 ]; then
		run sh -c "cp '$KEY_PUB' '$STATE/key.pub'"
	else
		cp "$KEY_PUB" "$STATE/key.pub"
		: >"$STATE/key"                        # marker: the private half lives with the user
		KEYOPT=""                              # ssh falls back to the user's default identities
		SSH_OPTS="$KEYOPT -p $SSHPORT $SSH_EXTRA"
		stamp key
	fi
	note "key: authorized_keys from $KEY_PUB (no ssh-keygen needed; ssh must hold the matching private key)"
elif stamped key && [ -f "$STATE/key" ]; then
	note "key: already done"
else
	if ! command -v ssh-keygen >/dev/null 2>&1; then
		if [ "$DRY_RUN" = 1 ]; then
			# dry-run prints every step even where a real run would stop
			note "key: ssh-keygen MISSING — a real run fails here (exit 10); install openssh-client or pass --key /path/to/key.pub"
			run ssh-keygen -q -t ed25519 -N '' -C akuma-install -f "$STATE/key"
		else
			fatal 10 key "ssh-keygen missing: apt install openssh-client / apk add openssh-client / brew install openssh-client — or rerun with --key /path/to/key.pub"
		fi
	elif [ "$DRY_RUN" = 1 ]; then
		run ssh-keygen -q -t ed25519 -N '' -C akuma-install -f "$STATE/key"
	else
		if ! ( ssh-keygen -q -t ed25519 -N '' -C akuma-install -f "$STATE/key" ) >"$LOGS/key.log" 2>&1; then
			fatal 10 key "ssh-keygen failed"
		fi
		stamp key
	fi
	note "key: $STATE/key(.pub)"
fi

# stage image --------------------------------------------------------------
ROOT="$STATE/root"
if stamped image && [ -f "$IMG" ]; then
	note "image: already done ($IMG)"
else
	if [ "$DRY_RUN" = 1 ]; then
		run sh -c "mkdir -p '$ROOT/bin' '$ROOT/etc/sshd' '$ROOT/tmp' && cp '$SSHD_BIN' '$ROOT/bin/sshd' && cp '$BB' '$ROOT/bin/busybox' && ln -sf busybox '$ROOT/bin/sh' && cp -L '$STATE/key.pub' '$ROOT/etc/sshd/authorized_keys'"
		run mke2fs -q -F -t ext2 -b 4096 -d "$ROOT" "$IMG" 65536
		note "image: fallback order — mke2fs -d, else the README debugfs -w recipe, else busybox mke2fs + loop mount (root)"
	else
		mkdir -p "$ROOT/bin" "$ROOT/etc/sshd" "$ROOT/tmp"
		cp "$SSHD_BIN" "$ROOT/bin/sshd"
		cp "$BB" "$ROOT/bin/busybox"
		ln -sf busybox "$ROOT/bin/sh"
		cp -L "$STATE/key.pub" "$ROOT/etc/sshd/authorized_keys"
		chmod 755 "$ROOT/bin/sshd" "$ROOT/bin/busybox"
		rm -f "$IMG"
		(
			if mke2fs -q -F -t ext2 -b 4096 -d "$ROOT" "$IMG" 65536 2>&1; then
				echo "IMAGE-TOOL: mke2fs -d (native e2fsprogs)"
			elif command -v debugfs >/dev/null 2>&1; then
				rm -f "$IMG"
				# busybox mke2fs needs an existing file; real mke2fs creates it
				dd if=/dev/zero of="$IMG" bs=1048576 count=64 2>/dev/null || exit 1
				# real mke2fs first; busybox mke2fs has no -q/-t and gets plain flags
				mke2fs -q -F -t ext2 -b 4096 "$IMG" 65536 2>/dev/null || mke2fs -F -b 4096 "$IMG" 65536 || exit 1
				cat >"$STATE/debugfs.cmds" <<EOF
mkdir /bin
mkdir /etc
mkdir /etc/sshd
mkdir /tmp
write $ROOT/bin/sshd /bin/sshd
sif /bin/sshd mode 0100755
write $ROOT/bin/busybox /bin/busybox
sif /bin/busybox mode 0100755
symlink /bin/sh /bin/busybox
write $ROOT/etc/sshd/authorized_keys /etc/sshd/authorized_keys
EOF
				debugfs -w -f "$STATE/debugfs.cmds" "$IMG"
				echo "IMAGE-TOOL: README debugfs -w recipe"
			else
				# busybox mke2fs: no -t, no -d. Create the fs, then populate
				# through a loop mount. Root only; on a non-root host this
				# fails loudly instead of producing an empty image.
				rm -f "$IMG"
				dd if=/dev/zero of="$IMG" bs=1048576 count=64 2>/dev/null || exit 1
				mke2fs -F -b 4096 "$IMG" 65536 || exit 1
				mkdir -p "$STATE/mnt"
				if mount -t ext2 -o loop "$IMG" "$STATE/mnt"; then
					cp -a "$ROOT/." "$STATE/mnt/"
					umount "$STATE/mnt"
					echo "IMAGE-TOOL: busybox mke2fs + loop-mount fallback (root)"
				else
					echo "loop mount failed; install e2fsprogs (mke2fs -d + debugfs) and rerun" >&2
					exit 1
				fi
			fi
			ls -l "$IMG"
		) >"$LOGS/image.log" 2>&1 || fatal 40 image "image build failed (see $LOGS/image.log)"
		[ -f "$IMG" ] || fatal 40 image "image missing after stage"
		stamp image
		note "image: $IMG ($(awk '/IMAGE-TOOL:/{print $2; exit}' "$LOGS/image.log"))"
	fi
fi

# stage boot + wait --------------------------------------------------------
epoch=$(date +%s)
if [ "$VMM" = qemu ]; then
	ACCEL=tcg
	[ -w /dev/kvm ] && ACCEL=kvm
	[ "$ACCEL" = tcg ] && WAIT_DEADLINE=600
	APPEND="init=/bin/sshd initargs=--port,22 epoch=$epoch"
	[ "$SMP" != 1 ] && APPEND="smp=$SMP $APPEND"
	# the tree's virtio drivers speak only virtio v2; microvm defaults to legacy
	i=0
	while [ "$i" -lt 4 ]; do
		addr=$(printf '0x%x' $((503316480 + i * 512)))
		APPEND="$APPEND virtio_mmio.device=512@$addr:$((5 + i))"
		i=$((i+1))
	done
	OVERLAY="$STATE/overlay.qcow2"
	[ "$PERSIST" = 1 ] && OVERLAY="$IMG"
	QEMU_CMD="qemu-system-x86_64 -M microvm,accel=$ACCEL -cpu max -smp $SMP -m $MEM_MIB -kernel $KERNEL_BIN -append '$APPEND' -nodefaults -no-reboot -display none -serial file:$STATE/console.log -drive file=$OVERLAY,format=qcow2,if=none,id=d0 -global virtio-mmio.force-legacy=false -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$PORT-:22 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1 -pidfile $STATE/qemu.pid -daemonize"
	BOOT_STEPS="qemu-img create -f qcow2 -b $IMG -F raw $OVERLAY   # throwaway overlay unless --persist
$QEMU_CMD"
	WAIT_TARGET="root@127.0.0.1"
	BIN_NEEDED=qemu-system-x86_64
elif [ "$VMM" = akuma ]; then
	TAP=none
	AKSVC=akuma-install-$PORT
	VMDISK="$STATE/disk-run.img"                # private copy: never boot the stamped image
	[ "$PERSIST" = 1 ] && VMDISK="$IMG"
	AKCONF=$(printf '%s\n' "vm = true" "vm_type = kvm" "vm_kernel = $KERNEL_BIN" "vm_disk = $VMDISK" "vm_ram_mib = $MEM_MIB" "vm_vcpus = $SMP" "vm_forward = $PORT:22" "vm_cmdline = init=/bin/sshd initargs=--port,22 epoch=$epoch" "restart = false")
	BOOT_STEPS="cp $IMG $VMDISK   # throwaway copy
write /etc/herd/available/$AKSVC.conf and enabled/ with:
$AKCONF
herd disable $AKSVC; herd enable $AKSVC; herd start $AKSVC   # herd only re-parses on disable then enable; one VM at a time on kvm0"
	WAIT_TARGET="root@127.0.0.1"
	BIN_NEEDED=herd
else
	TAP=akuma-tap-$PORT
	GUEST_MAC=02:FC:00:00:00:07                 # private to this script; never another VM's
	GUEST_HOST=10.0.2.15                        # what the guest DHCPs to on the tap
	FCDISK="$STATE/disk-run.img"                # private copy: never boot the stamped image
	[ "$PERSIST" = 1 ] && FCDISK="$IMG"
	FC_JSON=$(printf '{"boot-source":{"kernel_image_path":"%s","boot_args":"console=ttyS0 init=/bin/sshd initargs=--port,22 epoch=%s"},"drives":[{"drive_id":"rootfs","path_on_host":"%s","is_root_device":true,"is_read_only":false,"io_engine":"Sync"}],"network-interfaces":[{"iface_id":"eth0","host_dev_name":"%s","guest_mac":"%s"}],"entropy":{},"machine-config":{"vcpu_count":%s,"mem_size_mib":%s}}' "$KERNEL_BIN" "$epoch" "$FCDISK" "$TAP" "$GUEST_MAC" "$SMP" "$MEM_MIB")
	BOOT_STEPS="ip tuntap add dev $TAP mode tap && ip addr add 10.0.2.2/24 dev $TAP && ip link set $TAP up
printf '%s\n' '$FC_JSON' > $STATE/fc.json
setsid nohup firecracker --no-api --config-file $STATE/fc.json > $STATE/console.log 2>&1 < /dev/null &   # pid -> $STATE/firecracker.pid"
	WAIT_TARGET="root@$GUEST_HOST"
	BIN_NEEDED=firecracker
fi

if stamped boot; then
	note "boot: already done (pid $(cat "$STATE/$VMM.pid" 2>/dev/null))"
else
	if [ "$DRY_RUN" = 1 ]; then
		printf 'DRY: boot stage (%s) would run:\n%s\n' "$VMM" "$BOOT_STEPS"
	else
		if ! command -v "$BIN_NEEDED" >/dev/null 2>&1; then
			printf 'DRY: boot stage (%s) would run:\n%s\n' "$VMM" "$BOOT_STEPS" >"$LOGS/boot.log"
			fatal 50 boot "$BIN_NEEDED not installed — this host cannot boot a guest; the exact boot command is in $LOGS/boot.log"
		fi
		if [ "$VMM" = firecracker ] && [ ! -w /dev/kvm ]; then
			printf 'DRY: boot stage (%s) would run:\n%s\n' "$VMM" "$BOOT_STEPS" >"$LOGS/boot.log"
			fatal 50 boot "/dev/kvm not writable — firecracker cannot start; the exact command is in $LOGS/boot.log"
		fi
		: >"$LOGS/boot.log"
		if [ "$VMM" = qemu ]; then
			if [ "$PERSIST" = 0 ]; then
				qemu-img create -f qcow2 -b "$IMG" -F raw "$OVERLAY" >>"$LOGS/boot.log" 2>&1 || fatal 50 boot "qemu-img overlay failed"
			fi
			# shellcheck disable=SC2086
			eval "$QEMU_CMD" >>"$LOGS/boot.log" 2>&1 || fatal 50 boot "qemu did not start"
		elif [ "$VMM" = akuma ]; then
			# Akuma's own hypervisor (/dev/kvm0) through herd: the one guest path on Akuma that has a network
			# (vm_forward). kvm0 runs one VM at a time: refuse to touch somebody else's.
			other=$(herd status 2>/dev/null | grep '(vm)' | grep ': running' | grep -v "$AKSVC" | head -3)
			[ -z "$other" ] || fatal 50 boot "another VM is running on kvm0 ($other) and it allows one: a human stops it first (herd stop <name>), then rerun"
			if [ "$PERSIST" = 0 ]; then cp "$IMG" "$VMDISK" || fatal 50 boot "could not copy the image to $VMDISK"; fi
			printf '%s\n' "$AKCONF" >"/etc/herd/available/$AKSVC.conf" || fatal 50 boot "could not write the herd conf"
			cp "/etc/herd/available/$AKSVC.conf" "/etc/herd/enabled/$AKSVC.conf" || fatal 50 boot "could not enable the herd conf"
			herd disable "$AKSVC" >>"$LOGS/boot.log" 2>&1 || :
			herd enable "$AKSVC" >>"$LOGS/boot.log" 2>&1 || fatal 50 boot "herd enable failed"
			echo "$AKSVC" >"$STATE/akuma.svc"
			# a new service is only known after herd's next config reload (within ~20 s)
			i=0
			while [ "$i" -lt 20 ]; do
				herd status 2>/dev/null | grep -q "^ *$AKSVC:" && break
				sleep 2; i=$((i+1))
			done
			herd start "$AKSVC" >>"$LOGS/boot.log" 2>&1 || fatal 50 boot "herd start $AKSVC failed (see $LOGS/boot.log)"
			sleep 3
			herd status 2>/dev/null | grep -q "^ *$AKSVC: running" || fatal 50 boot "herd service $AKSVC is not running: $(herd status 2>&1 | grep "$AKSVC")"
		else
				# Firecracker has no user-mode network, so the guest needs a tap, and creating one needs root.
				# This script never uses sudo: it uses a tap that already exists, creates one only if it is
				# itself running as root, and otherwise stops and prints the one command an administrator runs once.
				IPBIN=$(command -v ip || echo /sbin/ip)
				if "$IPBIN" link show "$TAP" >/dev/null 2>&1; then
					note "boot: using existing tap $TAP"
				elif [ "$(id -u)" = 0 ]; then
					"$IPBIN" tuntap add dev "$TAP" mode tap >>"$LOGS/boot.log" 2>&1 || fatal 50 boot "could not create $TAP"
					"$IPBIN" addr add 10.0.2.2/24 dev "$TAP" >>"$LOGS/boot.log" 2>&1 || fatal 50 boot "could not address $TAP"
					"$IPBIN" link set "$TAP" up >>"$LOGS/boot.log" 2>&1
					echo "$TAP" >"$STATE/fc.tap"          # created by this script: down removes it
				else
					fatal 50 boot "tap $TAP does not exist and this script does not use sudo. A human administrator runs ONCE: sudo ip tuntap add dev $TAP mode tap user $(id -un) && sudo ip addr add 10.0.2.2/24 dev $TAP && sudo ip link set $TAP up   -- then rerun (do not run it yourself unless the user told you to)"
				fi
				if [ "$PERSIST" = 0 ]; then cp "$IMG" "$FCDISK" || fatal 50 boot "could not copy the image to $FCDISK"; fi   # throwaway copy, like qemu's overlay
				printf '%s\n' "$FC_JSON" >"$STATE/fc.json" || fatal 50 boot "could not write $STATE/fc.json"
				cat "$STATE/fc.json" >>"$LOGS/boot.log" 2>&1
				# launch like scripts/benchmarks/ryzen_fc/fcrun.sh: own session, own pid file
				( setsid nohup firecracker --no-api --config-file "$STATE/fc.json" >"$STATE/console.log" 2>&1 </dev/null & echo $! >"$STATE/firecracker.pid" )
				sleep 2
				kill -0 "$(cat "$STATE/firecracker.pid")" 2>/dev/null || fatal 50 boot "firecracker exited at start: $(tail -c 300 "$STATE/console.log" | tr -d '\000')"
		fi
		printf '%s' "$VMM" >"$STATE/vmm"
		stamp boot
	fi
	note "boot: $VMM ($([ "$VMM" = qemu ] && echo "accel=$ACCEL hostfwd 127.0.0.1:$PORT->22" || echo "tap=$TAP guest=$GUEST_HOST")) console=$STATE/console.log"
fi

# stage wait: readiness is an ssh round-trip, never a log grep
t0=$(date +%s)
forget_host
while :; do
	now=$(date +%s)
	if [ $((now - t0)) -ge "$WAIT_DEADLINE" ]; then
		fatal 60 wait "guest never answered ssh within ${WAIT_DEADLINE}s (still booting? see $STATE/console.log)"
	fi
	if [ "$DRY_RUN" = 1 ]; then
		run sh -c "$SSH_YES ssh $SSH_OPTS $WAIT_TARGET true"
		break
	fi
	# a VM that died will never answer: say so now instead of at the deadline
	pidf="$STATE/$VMM.pid"
	if [ -f "$pidf" ] && ! kill -0 "$(cat "$pidf")" 2>/dev/null; then
		fatal 50 boot "$VMM exited before ssh answered: $(tail -c 300 "$STATE/console.log" 2>/dev/null | tr -d '\000')"
	fi
	# shellcheck disable=SC2086
	if { [ "$SSH_FLAVOR" = akuma ] && echo yes || true; } | ssh $SSH_OPTS "$WAIT_TARGET" true >>"$LOGS/wait.log" 2>&1; then
		stamp wait
		break
	fi
	sleep 3
done

printf 'AKUMA_READY=1\n'
printf 'AKUMA_SSH=ssh %s -p %s -o StrictHostKeyChecking=no %s\n' "${KEYOPT:--i $DIR/state/key}" "$SSHPORT" "$WAIT_TARGET"
printf 'AKUMA_STOP=sh %s down\n' "$SCRIPT_PATH"
exit 0
