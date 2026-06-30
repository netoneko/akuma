#!/bin/bash
# Populate disk.img with bootstrap files using Docker
# This avoids needing fuse-ext2 or debugfs on macOS
#
# Usage: ./scripts/populate_disk.sh [--bin-only] [--with-apk] [--with-musl-dev] [--with-rust-toolchain]
#   --bin-only             Only update /bin directory (faster for development)
#   --etc-only             Only update /etc directory (faster for development)
#   --with-apk             Pre-install Alpine busybox package (sets up symlinks via apk)
#   --with-musl-dev        Pre-install musl-dev (C headers + static libs) and extract
#                          libtcc1.tar — disk boots ready for tcc -static without apk add
#   --with-rust-toolchain  Pre-install a full nightly Rust toolchain (aarch64 musl host)
#                          into the disk for self-hosting akuma. Downloads rustc, cargo,
#                          host rust-std, the aarch64-unknown-none target std, and rust-src
#                          from static.rust-lang.org and installs them under /usr/local;
#                          also apk-installs the C toolchain (clang, lld, gcc, make,
#                          musl-dev). No network needed inside the VM to start a build.
#
# DISK env var overrides the image path (default: disk.img), so you can prepare a
# separate image without touching the primary one, e.g.:
#   DISK=disk_selfhost.img scripts/populate_disk.sh --with-apk --with-musl-dev --with-rust-toolchain

set -e

DISK_IMG="${DISK:-disk.img}"
BOOTSTRAP_DIR="bootstrap/"
BIN_ONLY=false
WITH_APK=false
WITH_MUSL_DEV=false
WITH_RUST_TOOLCHAIN=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --bin-only)
            BIN_ONLY=true
            shift
            ;;
        --etc-only)
            ETC_ONLY=true
            shift
            ;;
        --with-apk)
            WITH_APK=true
            shift
            ;;
        --with-musl-dev)
            WITH_MUSL_DEV=true
            shift
            ;;
        --with-rust-toolchain)
            WITH_RUST_TOOLCHAIN=true
            shift
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--bin-only] [--with-apk] [--with-musl-dev] [--with-rust-toolchain]"
            exit 1
            ;;
    esac
done

if [ ! -f "$DISK_IMG" ]; then
    echo "Error: $DISK_IMG not found. Run ./scripts/create_disk.sh first."
    exit 1
fi

if [ ! -d "$BOOTSTRAP_DIR" ]; then
    echo "Error: $BOOTSTRAP_DIR directory not found."
    exit 1
fi

if [ "$BIN_ONLY" = true ]; then
    echo "Updating /bin in $DISK_IMG..."
    COPY_CMD='
        # Only copy bin directory
        echo "Copying bin/..."
        rm -rf /mnt/disk/bin/*
        cp -rv /bootstrap/bin/* /mnt/disk/bin/

        # List bin contents
        echo ""
        echo "/bin contents:"
        ls -la /mnt/disk/bin/
    '
 elif [ "$ETC_ONLY" = true ]; then
    echo "Updating /etc in $DISK_IMG..."
    COPY_CMD='
        # Only copy etc directory
        echo "Copying etc/..."
        rm -rf /mnt/disk/etc/*
        cp -rv /bootstrap/etc/* /mnt/disk/etc/

        # List bin contents
        echo ""
        echo "/bin contents:"
        ls -la /mnt/disk/etc/
    '
 else
    echo "Populating $DISK_IMG with contents of $BOOTSTRAP_DIR..."
    COPY_CMD='
        # Wipe /tmp so VM-generated artifacts from prior runs do not persist.
        # bootstrap/tmp/ is re-staged by the cp below.
        rm -rf /mnt/disk/tmp

        # Copy all bootstrap files
        echo "Copying files..."
        cp -rv /bootstrap/* /mnt/disk/

        # List contents
        echo ""
        echo "Disk contents:"
        ls -la /mnt/disk/
    '
fi

# Build the apk pre-install command (runs inside the Docker container after copy)
APK_CMD=''
if [ "$WITH_APK" = true ] || [ "$WITH_MUSL_DEV" = true ]; then
    APK_PKGS="busybox-static"
    if [ "$WITH_MUSL_DEV" = true ]; then
        APK_PKGS="$APK_PKGS musl-dev"
    fi
    # apk --root reads /mnt/disk/etc/apk/arch (aarch64) and installs the right packages.
    # --no-scripts skips post-install triggers that would try to run aarch64 binaries.
    APK_CMD="
        echo 'Installing Alpine packages: $APK_PKGS ...'
        apk --root /mnt/disk --no-scripts add $APK_PKGS
    "
    if [ "$WITH_MUSL_DEV" = true ]; then
        APK_CMD="$APK_CMD
        echo 'Extracting libtcc1 runtime...'
        tar xf /mnt/disk/archives/libtcc1.tar -C /mnt/disk
        "
    fi
fi

# Build the nightly Rust toolchain install command (runs inside the container after copy).
# Akuma runs musl userspace, so we install the aarch64-unknown-linux-musl HOST toolchain
# (a glibc rustc binary would not run on Akuma). A release kernel build does not need
# build-std (profile.release = panic="abort" uses the precompiled aarch64-unknown-none
# std), but we ship rust-src anyway so the size/extreme profiles (panic="immediate-abort")
# can build-std too.
RUST_CMD=''
if [ "$WITH_RUST_TOOLCHAIN" = true ]; then
    RUST_CMD='
        echo "=== Installing nightly Rust toolchain into the disk ==="
        RUST_HOST=aarch64-unknown-linux-musl
        DIST=https://static.rust-lang.org/dist
        PREFIX=/mnt/disk/usr/local

        # Host-side tools needed to fetch/unpack/install (these live in the container,
        # not the disk). bash is required by the rust components install.sh.
        apk add --no-cache curl xz bash >/dev/null

        # C toolchain INTO the disk (clang/lld for linking, gcc/binutils/make for cc-rs build
        # scripts, musl-dev for headers + static libs).
        echo "Installing C toolchain (clang lld gcc binutils make musl-dev) into disk..."
        apk --root /mnt/disk --no-scripts add clang lld gcc binutils make musl-dev

        mkdir -p /tmp/rust
        for comp in \
            rustc-nightly-$RUST_HOST \
            cargo-nightly-$RUST_HOST \
            rust-std-nightly-$RUST_HOST \
            rust-std-nightly-aarch64-unknown-none \
            rust-src-nightly ; do
            echo "Downloading $comp ..."
            curl -fsSL "$DIST/$comp.tar.xz" -o /tmp/rust/$comp.tar.xz
            echo "Extracting $comp ..."
            # Decompress with the real xz binary (busybox tar -J chokes on these
            # large/modern .xz streams: "corrupted data / short read").
            xz -dc /tmp/rust/$comp.tar.xz | tar x -C /tmp/rust
            echo "Installing $comp -> $PREFIX ..."
            (cd /tmp/rust/$comp && ./install.sh --prefix="$PREFIX" --disable-ldconfig)
            rm -rf /tmp/rust/$comp /tmp/rust/$comp.tar.xz
        done

        # PATH for the in-VM shell (best-effort; the playbook also sets it explicitly).
        mkdir -p /mnt/disk/etc/profile.d
        printf "export PATH=/usr/local/bin:\$PATH\n" > /mnt/disk/etc/profile.d/rust.sh

        echo "Rust toolchain installed:"
        ls /mnt/disk/usr/local/bin
    '
fi

# Use Docker to mount and copy files
# - Mount disk.img as a loop device inside the container
# - Copy bootstrap files to the mounted filesystem
docker run --rm --privileged \
    -v "$(pwd)/$DISK_IMG:/disk.img" \
    -v "$(pwd)/$BOOTSTRAP_DIR:/bootstrap:ro" \
    alpine:latest \
    sh -c "
        set -e

        # Install e2fsprogs for ext2 support (alpine uses busybox mount which supports ext2)
        echo 'Setting up mount...'

        # Create mount point
        mkdir -p /mnt/disk

        # Mount the disk image (loop device)
        mount -o loop /disk.img /mnt/disk

        $COPY_CMD

        $APK_CMD

        $RUST_CMD

        # Create git -> scratch symlink so 'git clone' works without specifying scratch
        ln -sf scratch /mnt/disk/bin/git
        echo 'Created /bin/git -> scratch'

        # Create essential busybox symlinks (apk --no-scripts skips post-install triggers)
        for cmd in sh chmod ls mkdir rm cat echo grep; do
            ln -sf busybox.static /mnt/disk/bin/$cmd 2>/dev/null || true
        done
        echo 'Created busybox symlinks'

        # Sync and unmount
        sync
        umount /mnt/disk

        echo ''
        echo 'Done!'
    "

echo ""
if [ "$BIN_ONLY" = true ]; then
    echo "Successfully updated /bin in $DISK_IMG"
elif [ "$WITH_RUST_TOOLCHAIN" = true ]; then
    echo "Successfully populated $DISK_IMG with the nightly Rust toolchain + packages"
elif [ "$WITH_APK" = true ] || [ "$WITH_MUSL_DEV" = true ]; then
    echo "Successfully populated $DISK_IMG with pre-installed packages"
else
    echo "Successfully populated $DISK_IMG"
fi

