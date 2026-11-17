#!/usr/bin/env bash
# Stage a direct-EC2 rootfs. The destination must not already exist.
set -euo pipefail
if [ "$#" -ne 3 ]; then
  echo "usage: $0 BASE_ROOT NEW_DEST x86_64|aarch64" >&2
  exit 2
fi
base=$(realpath "$1")
dest=$2
arch=$3
case "$arch" in
  x86_64) platform=linux/amd64 ;;
  aarch64) platform=linux/arm64 ;;
  *) echo "unsupported architecture: $arch" >&2; exit 2 ;;
esac
[ -d "$base" ] || { echo 'base root is not a directory' >&2; exit 1; }
[ ! -e "$dest" ] || { echo 'destination already exists' >&2; exit 1; }
for bin in herd sshd sh; do
  [ -f "$base/bin/$bin" ] || { echo "base root is missing /bin/$bin" >&2; exit 1; }
done
mkdir -p "$dest"
dest=$(realpath "$dest")
case "$dest/" in "$base/"*) echo 'destination must be outside base root' >&2; exit 1;; esac
script_dir=$(cd "$(dirname "$0")" && pwd)
# Keep apk package signatures and the target's future package trust roots.
# Base files are copied afterwards to preserve Akuma's own shell and binaries.
docker run --rm --platform "$platform" -v "$dest:/target" \
  -e TARGET_UID="$(id -u)" -e TARGET_GID="$(id -g)" alpine:3.20 sh -ec '
  apk add --root /target --initdb --no-cache --no-scripts --keys-dir /etc/apk/keys \
    --repository https://dl-cdn.alpinelinux.org/alpine/v3.20/main \
    apk-tools curl ca-certificates-bundle
  mkdir -p /target/etc/apk/keys
  cp /etc/apk/keys/* /target/etc/apk/keys/
  cp /etc/apk/repositories /target/etc/apk/repositories
  chown -R "$TARGET_UID:$TARGET_GID" /target
'
cp "$dest/lib/apk/db/installed" "$dest/alpine-packages.manifest"
cp -a --remove-destination "$base/." "$dest/"
cp -a "$script_dir/rootfs/." "$dest/"
mkdir -p "$dest/etc/sshd"
mkdir -p "$dest/usr/share/licenses/akuma" "$dest/usr/share/licenses/aws-ena"
cp "$script_dir/../../LICENSE" "$dest/usr/share/licenses/akuma/LICENSE"
cp "$script_dir/../../crates/akuma-net-ena/AWS-LICENSE" "$dest/usr/share/licenses/aws-ena/LICENSE"
# Alpine records each installed package, version, upstream URL and license ID
# in its database. Preserve a separate copy for the image notice inventory.
mv "$dest/alpine-packages.manifest" "$dest/usr/share/licenses/alpine-installed-packages"
cp "$dest/usr/share/licenses/alpine-installed-packages" "$dest/lib/apk/db/installed"
rm -f "$dest/etc/sshd/authorized_keys" "$dest/etc/sshd/id_ed25519" \
  "$dest/etc/sshd/id_ed25519.pub" "$dest/etc/sshd/host_key"
[ -x "$dest/sbin/apk" ] && [ -x "$dest/usr/bin/curl" ] || {
  echo 'apk or curl is absent from the staged root' >&2; exit 1;
}
printf 'Staged %s rootfs at %s\n' "$arch" "$dest"
