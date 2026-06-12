#!/bin/bash
# Populate disk.img with bootstrap files using Docker
# This avoids needing fuse-ext2 or debugfs on macOS
#
# Usage: ./scripts/populate_disk.sh [--bin-only] [--with-apk] [--with-musl-dev]
#   --bin-only       Only update /bin directory (faster for development)
#   --with-apk       Pre-install Alpine busybox package (sets up symlinks via apk)
#   --with-musl-dev  Pre-install musl-dev (C headers + static libs) and extract
#                    libtcc1.tar — disk boots ready for tcc -static without apk add

set -e

DISK_IMG="disk.img"
BOOTSTRAP_DIR="bootstrap/"
BIN_ONLY=false
WITH_APK=false
WITH_MUSL_DEV=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --bin-only)
            BIN_ONLY=true
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
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--bin-only] [--with-apk] [--with-musl-dev]"
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
else
    echo "Populating $DISK_IMG with contents of $BOOTSTRAP_DIR..."
    COPY_CMD='
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
    APK_PKGS="busybox"
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

        # Sync and unmount
        sync
        umount /mnt/disk

        echo ''
        echo 'Done!'
    "

echo ""
if [ "$BIN_ONLY" = true ]; then
    echo "Successfully updated /bin in $DISK_IMG"
elif [ "$WITH_APK" = true ] || [ "$WITH_MUSL_DEV" = true ]; then
    echo "Successfully populated $DISK_IMG with pre-installed packages"
else
    echo "Successfully populated $DISK_IMG"
fi

