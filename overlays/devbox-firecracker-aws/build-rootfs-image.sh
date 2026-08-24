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

  # This profile takes /etc from the overlay ALONE, and the overlay carries no TLS
  # trust store -- so `curl https://...` in the guest could not verify any peer.
  # The bundle is arch-independent and lives in bootstrap/etc/ssl, which this
  # profile otherwise never reads, so lift exactly that one file rather than
  # widening what /etc inherits.
  if [ -f bootstrap/etc/ssl/certs/ca-certificates.crt ]; then
    say "etc/ssl/certs/ca-certificates.crt <- bootstrap/etc/ssl"
    mkdir -p "$ROOT/etc/ssl/certs"
    cp -a bootstrap/etc/ssl/certs/ca-certificates.crt "$ROOT/etc/ssl/certs/"
  fi
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

# ---------------------------------------------------------------------------
# 1b. busybox applet symlinks
# ---------------------------------------------------------------------------
# bootstrap/bin ships the busybox BINARY and not one of its applet links, so an
# image built straight from it has no ls, ps, cat or uname: on a normal system
# every one of those IS a symlink to busybox. The symptom is a guest that boots,
# accepts ssh, and answers every command with "not found" -- which reads as an
# empty disk rather than as missing links.
#
# overlays/devbox/bootstrap.sh already does this when it builds the QEMU disk
# (step 4); this is the same step for the image path, and the two lists are
# deliberately identical.
#
# `busybox --list` is authoritative, but it only runs when the workstation shares
# the guest's architecture -- on macOS the aarch64 binary cannot execute -- so
# fall back to the same static list bootstrap.sh carries.
BB=busybox.static
[ -e "$ROOT/bin/$BB" ] || BB=busybox
if [ -e "$ROOT/bin/$BB" ]; then
  # 1. Ask the binary itself. Works when the workstation shares the guest's
  #    architecture (i.e. building on the aarch64 host).
  APPLETS="$("$ROOT/bin/$BB" --list 2>/dev/null || true)"
  APPLET_SRC="--list"

  # 2. On a foreign-arch workstation the exec above fails, but docker can run the
  #    aarch64 binary under emulation -- and image mode already requires docker.
  #    This matters: the static list below is a guess maintained by hand, and it
  #    omitted `ls`, so an image built from it had no ls at all.
  if [ -z "$APPLETS" ] && command -v docker >/dev/null 2>&1; then
    APPLETS="$(docker run --rm --platform linux/arm64 \
      -v "$ROOT/bin:/bb:ro" alpine:latest /bb/"$BB" --list 2>/dev/null || true)"
    APPLET_SRC="docker --list"
  fi

  if [ -n "$APPLETS" ]; then
    say "busybox applet links: $APPLET_SRC ($(echo "$APPLETS" | wc -w | tr -d ' ') applets)"
  else
    say "WARNING: could not read the applet list; falling back to a static one."
    say "         It is hand-maintained and WILL be missing applets this busybox has."
    APPLETS="ls echo awk sed grep egrep fgrep find xargs tar gzip gunzip zcat bzip2 xz unxz cpio \
        timeout nohup dmesg pidof sysctl unlink mountpoint su unzip ip \
        less more head tail sort uniq wc cut tr nl fold comm paste \
        cat tac printf dir cp mv rm mkdir rmdir ln touch stat readlink realpath \
        basename dirname pwd env printenv date sleep usleep sync truncate \
        chmod chown chgrp du df mount umount losetup mknod mkfifo \
        ps top kill killall pgrep pkill nice renice free uptime watch \
        id whoami who groups hostname uname clear reset tty stty \
        test true false yes seq expr dd shuf \
        sha1sum sha256sum sha512sum md5sum cksum base64 \
        tee od hexdump xxd cmp diff patch strings split \
        which whereis mktemp getopt \
        wget nc telnet ping traceroute nslookup ifconfig route netstat \
        ash hush vi"
  fi
  LINKED=0
  for app in $APPLETS; do
    # Never clobber a real binary this image ships (git, tcc, meow, curl,
    # scratch, ...) -- only fill in the gaps.
    if [ -e "$ROOT/bin/$app" ] && [ ! -L "$ROOT/bin/$app" ]; then
      continue
    fi
    ln -sf "$BB" "$ROOT/bin/$app" 2>/dev/null && LINKED=$((LINKED + 1)) || true
  done
  say "busybox applet links: $LINKED created"
else
  say "WARNING: no busybox in bootstrap/bin -- the guest will have no shell utilities"
fi

mkdir -p "$ROOT"/{tmp,var,dev,proc,root}
chmod 1777 "$ROOT/tmp"

# ---------------------------------------------------------------------------
# 1c. Alpine packages: git, the C toolchain, and the apk database itself
# ---------------------------------------------------------------------------
# Ported from overlays/devbox/bootstrap.sh steps 5-7, which is the image that is
# known to build a kernel in-guest. Without these the guest has:
#   * no apk database  -> `apk add` fails with "Unable to lock database:
#     No such file or directory" before it does anything
#   * no real git      -> /bin/git is the `scratch` stub, whose clone dies with
#     "DNS resolution failed" even though the OS resolver answers fine
#   * no C toolchain   -> cargo build scripts cannot compile or link
#
# The devbox loop-mounts its ext2 image to do this; here $ROOT is a plain
# directory, so a bind mount is enough and no --privileged is needed.
#
# --platform linux/arm64 is NOT metadata here (unlike the Dockerfile build):
# apk resolves packages for the running container arch, so on an x86 or Apple
# silicon workstation this must run as arm64 or the guest gets foreign binaries.
#
# ONE `apk add` transaction, deliberately. Separate calls reset apk's "wanted"
# set and purge the previous step's packages -- the war story is in
# overlays/devbox/bootstrap.sh step 6.
if [ "${DEVBOX_APK_PACKAGES:-true}" = "true" ]; then
  command -v docker >/dev/null 2>&1 \
    || die "apk staging needs docker (set DEVBOX_APK_PACKAGES=false to skip, but the
       guest will have no apk database, no git and no C toolchain)"

  PKGS="ca-certificates-bundle musl git"
  [ "${DEVBOX_RUST_TOOLCHAIN:-true}" = "true" ] && PKGS="$PKGS clang lld gcc binutils make musl-dev"
  [ "${DEVBOX_STABLE_RUST:-false}" = "true" ]   && PKGS="$PKGS rust cargo"

  say "apk add ($PKGS)"
  docker run --rm --platform linux/arm64 \
    -v "$ROOT:/target" -e PKGS="$PKGS" alpine:latest \
    sh -c '
      set -e
      [ -n "$PKGS" ] || { echo "package list did not reach the container" >&2; exit 1; }
      apk add --root /target --initdb --no-cache --no-scripts --allow-untrusted \
        --repository https://dl-cdn.alpinelinux.org/alpine/latest-stable/main \
        $PKGS
      # Replace the base tree symlink git -> scratch. scratch stays reachable at
      # /bin/scratch; it is simply no longer what `git` means.
      [ -x /target/usr/bin/git ] && ln -sf /usr/bin/git /target/bin/git
      echo "git: $(readlink /target/bin/git 2>/dev/null || echo MISSING)"
    '
fi

# ---------------------------------------------------------------------------
# 1d. Nightly Rust, straight from static.rust-lang.org, into /usr/local
# ---------------------------------------------------------------------------
# overlays/devbox/bootstrap.sh step 7b. This is THE toolchain on a devbox image:
# the kernel PATH default (akuma_exec::process::types::DEFAULT_ENV) puts
# /usr/local ahead of /usr, so plain `cargo`/`rustc` resolve here. No profile.d
# script is written -- the devbox proved that one never runs (non-login shells).
#
# aarch64-unknown-none is included because that is what the kernel builds for.
# This is a large download and roughly doubles the image; DEVBOX_NIGHTLY_RUST=false
# skips it, which on a default image leaves NO Rust at all.
if [ "${DEVBOX_NIGHTLY_RUST:-true}" = "true" ]; then
  command -v docker >/dev/null 2>&1 || die "nightly Rust staging needs docker"
  say "nightly Rust -> /usr/local (large download)"
  docker run --rm --platform linux/arm64 -v "$ROOT:/target" alpine:latest \
    sh -c '
      set -e
      apk add --no-cache curl xz bash >/dev/null
      RUST_HOST=aarch64-unknown-linux-musl
      DIST=https://static.rust-lang.org/dist
      PREFIX=/target/usr/local
      mkdir -p /tmp/rust "$PREFIX"
      for comp in \
          rustc-nightly-$RUST_HOST \
          cargo-nightly-$RUST_HOST \
          rust-std-nightly-$RUST_HOST \
          rust-std-nightly-aarch64-unknown-none \
          rust-src-nightly ; do
        echo "  $comp"
        curl -fsSL "$DIST/$comp.tar.xz" -o /tmp/rust/$comp.tar.xz
        xz -dc /tmp/rust/$comp.tar.xz | tar x -C /tmp/rust
        (cd /tmp/rust/$comp && ./install.sh --prefix="$PREFIX" --disable-ldconfig >/dev/null)
        rm -rf /tmp/rust/$comp /tmp/rust/$comp.tar.xz
      done
      ls "$PREFIX/bin"
    '
fi

# ---------------------------------------------------------------------------
# 1e. Cargo network policy, shipped with the toolchain
# ---------------------------------------------------------------------------
# overlays/devbox/bootstrap.sh step 7c. retry = 20 is the load-bearing setting:
# static.crates.io download connections fail often enough on this stack that
# cargo default budget of 3 aborts a large fetch part-way. `[net] offline` is
# deliberately NOT pinned -- that is a per-command choice.
if [ "${DEVBOX_NIGHTLY_RUST:-true}" = "true" ] || [ "${DEVBOX_STABLE_RUST:-false}" = "true" ]; then
  say "cargo network policy -> /root/.cargo/config.toml"
  mkdir -p "$ROOT/root/.cargo"
  cat > "$ROOT/root/.cargo/config.toml" <<'CARGOCFG'
# Shipped by overlays/devbox-firecracker-aws/build-rootfs-image.sh, with the
# toolchain. Mirrors overlays/devbox/bootstrap.sh step 7c.
# Rationale, and the failure this does NOT fix:
#   docs/runbooks/cargo-cannot-reach-crates-io.md
[net]
retry = 20

[http]
multiplexing = false
CARGOCFG
fi


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
