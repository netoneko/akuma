#!/bin/sh
# Laptop side: ship the unpushed working tree to ryzen as $W/local.tar, which
# build.sh unpacks over its fresh clone. Tracked-and-modified plus untracked
# (not ignored) files under the paths a ryzen build reads. Then run there:
#   runuser -u netoneko -- sh $W/akuma/overlays/ryzen/build.sh   (or the copy in $W)
set -e
cd "$(git rev-parse --show-toplevel)"
W=/home/netoneko/akuma-metal
FILES=$(git ls-files -m -o --exclude-standard -- amd64 crates userspace/herd userspace/wifi userspace/Cargo.toml userspace/Cargo.lock Cargo.toml Cargo.lock overlays/ryzen | sort -u)
[ -n "$FILES" ] || { echo "nothing to send"; exit 0; }
echo "$FILES"
# shellcheck disable=SC2086
COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata -cf - $FILES | ssh ryzen "cat > $W/local.tar && chown netoneko: $W/local.tar && tar -tf $W/local.tar | wc -l"
ssh ryzen "cp /dev/stdin $W/build.sh" < overlays/ryzen/build.sh
ssh ryzen "cp /dev/stdin $W/install.sh" < overlays/ryzen/install.sh
