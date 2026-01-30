#!/bin/bash
# Copy src and userspace directories to disk.img at /public/
# This avoids needing fuse-ext2 or debugfs on macOS
#
# Usage: ./scripts/copy_src.sh

set -e

DISK_IMG="disk.img"
SRC_DIR="src"
USERSPACE_DIR="userspace"
DOCS_DIR="docs"

if [ ! -f "$DISK_IMG" ]; then
    echo "Error: $DISK_IMG not found. Run ./scripts/create_disk.sh first."
    exit 1
fi

if [ ! -d "$SRC_DIR" ]; then
    echo "Error: $SRC_DIR directory not found."
    exit 1
fi

if [ ! -d "$USERSPACE_DIR" ]; then
    echo "Error: $USERSPACE_DIR directory not found."
    exit 1
fi

echo "Copying $SRC_DIR and $USERSPACE_DIR to $DISK_IMG at /public/akuma/..."

# Use Docker to mount and copy files
# - Mount disk.img as a loop device inside the container
# - Copy src and userspace directories to /public/ on the mounted filesystem
docker run --rm --privileged \
    -v "$(pwd)/$DISK_IMG:/disk.img" \
    -v "$(pwd)/$SRC_DIR:/src_dir:ro" \
    -v "$(pwd)/$DOCS_DIR:/docs_dir:ro" \
    -v "$(pwd)/$USERSPACE_DIR:/userspace_dir:ro" \
    alpine:latest \
    sh -c '
        set -e
        
        echo "Setting up mount..."
        
        # Create mount point
        mkdir -p /mnt/disk
        
        # Mount the disk image (loop device)
        mount -o loop /disk.img /mnt/disk
        
        # Create /public directory if it does not exist
        mkdir -p /mnt/disk/public/akuma
        
        # Copy src and userspace directories
        echo "Copying src directory..."
        cp -rv /src_dir /mnt/disk/public/akuma/src

        echo "Copying docs directory..."
        cp -rv /docs_dir /mnt/disk/public/akuma/docs
        
        # Delete userspace directory if it exists
        echo "Removing existing userspace directory..."
        rm -rf /mnt/disk/public/akuma/userspace/target
        
        # Copy userspace directory excluding target/
        echo "Copying userspace directory (excluding target/)..."
        apk add --no-cache rsync > /dev/null 2>&1
        rsync -av --exclude="target" /userspace_dir/ /mnt/disk/public/akuma/userspace/
        
        # List contents
        echo ""
        echo "Disk /public/ contents:"
        ls -la /mnt/disk/public/
        
        # Sync and unmount
        sync
        umount /mnt/disk
        
        echo ""
        echo "Done!"
    '

echo ""
echo "Successfully copied src and userspace to $DISK_IMG at /public/"
