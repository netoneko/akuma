#!/usr/bin/env bash
# Package the Akuma root filesystem for a remote Firecracker host: as an OCI
# image for a registry, or as a tar.xz for plain scp.
#
# Canonical location for this merge. The akuma-terraform helpers
# (bin/publish-rootfs.sh, bin/package-rootfs.sh) delegate here rather than
# reimplementing it, so which /etc wins and what a profile contains is decided in
# exactly one place -- this file, next to the trees it merges.
#
# NO DEFAULT REGISTRY. --registry is required for any image build: this script
# will not silently tag for Docker Hub, and it will not carry an account id in
# the repo. Pass it, or set AKUMA_REGISTRY.
#
# Usage:
#   build-rootfs-image.sh --registry <host>/<repo> [--push]
#   build-rootfs-image.sh --tarball [FILE]
#   build-rootfs-image.sh --stage-only DIR
#
# Options:
#   --registry REF   image repository, e.g. 1234.dkr.ecr.ap-northeast-1.amazonaws.com/netoneko/akuma
#                    or ghcr.io/you/akuma. Tagged <profile> and <profile>-<gitrev>.
#   --push           push after building (requires an existing docker login)
#   --tarball [F]    emit a tar.xz instead of an image (default: akuma-rootfs-<profile>.tar.xz)
#   --stage-only DIR merge into DIR and stop -- no image, no archive
#   --profile P      devbox (default) or full
#   --with-box       include bootstrap/srv, the rump box rootfs
#   --pubkey FILE    ed25519 public key for /etc/sshd/authorized_keys
#                    (default ~/.ssh/id_ed25519.pub, or AKUMA_SSH_PUBKEY)
#
# Profiles:
#   devbox  bootstrap/{bin,usr} + /etc from overlays/devbox/rootfs ONLY. Mirrors
#           overlays/devbox/bootstrap.sh step 3, which wipes the base /etc before
#           overlaying, so nothing from bootstrap/etc is inherited unreviewed.
#   full    bootstrap/{bin,usr,etc,root,public,tmp} -- the disk.img shape.
#
# models/ (508 MB of GGUF weights), music/ (479 MB) and archives/ (53 MB) are
# NEVER included. They are 987 MB of the 1.2 GB in bootstrap/ and no part of a
# bootable image; scp them separately if a test needs them.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PROFILE=devbox
WITH_BOX=0
REGISTRY="${AKUMA_REGISTRY:-}"
PUSH=0
MODE=""
TARBALL=""
STAGE_DIR=""
PUBKEY="${AKUMA_SSH_PUBKEY:-$HOME/.ssh/id_ed25519.pub}"

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --registry)   REGISTRY="$2"; shift ;;
    --push)       PUSH=1 ;;
    --tarball)    MODE=tarball
                  case "${2:-}" in -*|"") ;; *) TARBALL="$2"; shift ;; esac ;;
    --stage-only) MODE=stage; STAGE_DIR="$2"; shift ;;
    --profile)    PROFILE="$2"; shift ;;
    --with-box)   WITH_BOX=1 ;;
    --pubkey)     PUBKEY="$2"; shift ;;
    -h|--help)    usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
  shift
done

[ -n "$MODE" ] || MODE=image
case "$PROFILE" in devbox|full) ;; *) echo "--profile must be devbox or full" >&2; exit 1 ;; esac

say() { printf '\033[1;36m[rootfs-image]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[rootfs-image] %s\033[0m\n' "$*" >&2; exit 1; }

cd "$REPO_ROOT"
[ -d bootstrap/bin ] || die "bootstrap/bin missing -- run userspace/build.sh first"

if [ "$MODE" = image ] && [ -z "$REGISTRY" ]; then
  die "no registry. Pass --registry <host>/<repo> or set AKUMA_REGISTRY.
       There is deliberately no default: this script will not tag for Docker Hub
       by accident, and no account id is committed to this tree.
       For the no-registry path use --tarball."
fi

# ---------------------------------------------------------------------------
# 1. Stage the merged image root
# ---------------------------------------------------------------------------
if [ "$MODE" = stage ]; then
  ROOT="$STAGE_DIR"
  rm -rf "$ROOT"
else
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  ROOT="$WORK/root"
fi
mkdir -p "$ROOT"

# cp -a, not cp -r: bootstrap/bin is mostly busybox applet symlinks, and
# dereferencing them would multiply one 1 MB busybox by sixty.
say "bin/ + usr/ from bootstrap/"
cp -a bootstrap/bin "$ROOT/bin"
cp -a bootstrap/usr "$ROOT/usr"

if [ "$PROFILE" = devbox ]; then
  say "etc/ from overlays/devbox/rootfs (overlay only)"
  cp -a overlays/devbox/rootfs/etc "$ROOT/etc"
  [ -d overlays/devbox/rootfs/root ] && cp -a overlays/devbox/rootfs/root "$ROOT/root"
else
  say "etc/ from bootstrap/ (full profile)"
  cp -a bootstrap/etc "$ROOT/etc"
  for d in root public tmp; do
    [ -d "bootstrap/$d" ] && cp -a "bootstrap/$d" "$ROOT/$d"
  done
  [ -f bootstrap/hello.c ] && cp -a bootstrap/hello.c "$ROOT/"
fi

if [ "$WITH_BOX" = 1 ]; then
  # /srv/rumpbox is the rump box's own rootfs; it needs its etc/resolv.conf and CA
  # bundle for the box's DNS and HTTPS, both of which ship in the tree.
  say "srv/ (rump box rootfs)"
  cp -a bootstrap/srv "$ROOT/srv"
fi

mkdir -p "$ROOT"/{tmp,var,dev,proc,root}
chmod 1777 "$ROOT/tmp"

# ---------------------------------------------------------------------------
# 2. The ssh key
# ---------------------------------------------------------------------------
# overlays/devbox/rootfs/etc/sshd/sshd.conf sets disable_key_verification = false,
# so an image with no authorized_keys refuses every connection. Injected at build
# time rather than committed, because it is the operator's key.
if [ -f "$PUBKEY" ]; then
  grep -q '^ssh-ed25519 ' "$PUBKEY" \
    || die "$PUBKEY is not an ssh-ed25519 key. userspace/sshd (userspace/sshd/src/keys.rs)
       implements exactly one key type, so anything else is unparseable rather
       than merely weaker."
  mkdir -p "$ROOT/etc/sshd"
  tr -d '\r' < "$PUBKEY" > "$ROOT/etc/sshd/authorized_keys"
  say "etc/sshd/authorized_keys <- $PUBKEY"
else
  say "WARNING: $PUBKEY not found. sshd will reject every login unless"
  say "         disable_key_verification is set back to true."
fi

GITREV="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
DIRTY=""
[ -n "$(git status --porcelain 2>/dev/null)" ] && DIRTY="-dirty"

{
  echo "profile:    $PROFILE"
  echo "built:      $(date -u +%FT%TZ)"
  echo "git:        $GITREV$DIRTY"
  echo "with-box:   $WITH_BOX"
  echo "media:      excluded (models/, music/, archives/)"
  echo "authorized_keys: $([ -f "$ROOT/etc/sshd/authorized_keys" ] && echo yes || echo NO)"
} > "$ROOT/etc/akuma-image-manifest"

BYTES="$(du -sk "$ROOT" | awk '{print $1*1024}')"
say "image root: $(( BYTES / 1048576 )) MiB"

# ---------------------------------------------------------------------------
# 3. Emit
# ---------------------------------------------------------------------------
case "$MODE" in
  stage)
    say "staged at $ROOT (not archived)"
    ;;

  tarball)
    TARBALL="${TARBALL:-$REPO_ROOT/akuma-rootfs-$PROFILE.tar.xz}"
    # -6 with threads rather than -9: on already-stripped binaries -9 buys a few
    # percent for several times the wall clock.
    say "compressing -> $TARBALL"
    XZ_OPT="-6 -T0" tar -cJf "$TARBALL" -C "$ROOT" .
    say "$TARBALL ($(du -h "$TARBALL" | awk '{print $1}'))"
    say "ext2 image needs at least $(( BYTES / 1048576 + 64 )) MiB"
    ;;

  image)
    command -v docker >/dev/null || die "docker not found"
    TAG="$PROFILE"
    VTAG="$PROFILE-$GITREV$DIRTY"
    cp "$SCRIPT_DIR/Dockerfile" "$SCRIPT_DIR/.dockerignore" "$(dirname "$ROOT")/"

    # --platform linux/arm64 is metadata only: the Dockerfile has no RUN, so
    # nothing is executed and no emulation is involved even on an x86 workstation.
    say "docker build -> $REGISTRY:$VTAG"
    docker build --platform linux/arm64 \
      -t "$REGISTRY:$VTAG" -t "$REGISTRY:$TAG" "$(dirname "$ROOT")"

    if [ "$PUSH" = 1 ]; then
      say "pushing $REGISTRY:$VTAG and :$TAG"
      docker push "$REGISTRY:$VTAG"
      docker push "$REGISTRY:$TAG"
    else
      say "built but not pushed (--push to push; log in to $REGISTRY first)"
    fi
    ;;
esac
