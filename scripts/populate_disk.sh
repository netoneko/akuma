#!/bin/bash
# Populate disk.img with bootstrap files using Docker
# This avoids needing fuse-ext2 or debugfs on macOS
#
# Usage: ./scripts/populate_disk.sh

set -e

DISK_IMG="disk.img"
BOOTSTRAP_DIR="bootstrap/"

if [ ! -f "$DISK_IMG" ]; then
    echo "Error: $DISK_IMG not found. Run ./scripts/create_disk.sh first."
    exit 1
fi

if [ ! -d "$BOOTSTRAP_DIR" ]; then
    echo "Error: $BOOTSTRAP_DIR directory not found."
    exit 1
fi

echo "Populating $DISK_IMG with contents of $BOOTSTRAP_DIR..."

# Use Docker to mount and copy files
# - Mount disk.img as a loop device inside the container
# - Copy bootstrap files to the mounted filesystem
docker run --rm --privileged \
    -v "$(pwd)/$DISK_IMG:/disk.img" \
    -v "$(pwd)/$BOOTSTRAP_DIR:/bootstrap:ro" \
    alpine:latest \
    sh -c '
        set -e
        
        # Install e2fsprogs for ext2 support (alpine uses busybox mount which supports ext2)
        echo "Setting up mount..."
        
        # Create mount point
        mkdir -p /mnt/disk
        
        # Mount the disk image (loop device)
        mount -o loop /disk.img /mnt/disk
        
        # Copy bootstrap files
        echo "Copying files..."
        cp -rv /bootstrap/* /mnt/disk/
        
        # List contents
        echo ""
        echo "Disk contents:"
        ls -la /mnt/disk/
        
        # Sync and unmount
        sync
        umount /mnt/disk
        
        echo ""
        echo "Done!"
    '

echo ""
echo "Successfully populated $DISK_IMG"

