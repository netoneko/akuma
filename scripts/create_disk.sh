#!/bin/bash
# Create an ext2 disk image for the Akuma kernel
# Usage: ./scripts/create_disk.sh [size_mb]

set -e

SIZE_MB=${1:-128}
DISK_IMG="disk.img"

echo "Creating ${SIZE_MB}MB ext2 disk image..."

# Remove existing image if present
if [ -f "$DISK_IMG" ]; then
    echo "Removing existing $DISK_IMG"
    rm "$DISK_IMG"
fi

# Detect OS and use appropriate method
OS=$(uname -s)

# Create empty disk image
if [ "$OS" = "Darwin" ]; then
    dd if=/dev/zero of="$DISK_IMG" bs=1m count="$SIZE_MB"
else
    dd if=/dev/zero of="$DISK_IMG" bs=1M count="$SIZE_MB" status=progress
fi

# Format with ext2
if [ "$OS" = "Darwin" ]; then
    # macOS: Use e2fsprogs from Homebrew
    if command -v mkfs.ext2 &> /dev/null; then
        mkfs.ext2 -F -L "AKUMA" "$DISK_IMG"
    elif command -v /opt/homebrew/sbin/mkfs.ext2 &> /dev/null; then
        /opt/homebrew/sbin/mkfs.ext2 -F -L "AKUMA" "$DISK_IMG"
    elif command -v /usr/local/sbin/mkfs.ext2 &> /dev/null; then
        /usr/local/sbin/mkfs.ext2 -F -L "AKUMA" "$DISK_IMG"
    else
        echo ""
        echo "Error: mkfs.ext2 not found."
        echo "Please install e2fsprogs:"
        echo ""
        echo "  brew install e2fsprogs"
        echo ""
        echo "After installing, you may need to add it to your PATH or run:"
        echo "  /opt/homebrew/sbin/mkfs.ext2 -F -L AKUMA $DISK_IMG"
        echo ""
        # Create empty image anyway
        echo "Created empty disk image (not formatted)"
        exit 1
    fi
else
    # Linux: Use mkfs.ext2
    if command -v mkfs.ext2 &> /dev/null; then
        mkfs.ext2 -F -L "AKUMA" "$DISK_IMG"
    else
        echo "Error: mkfs.ext2 not found. Install e2fsprogs."
        echo "  Ubuntu/Debian: sudo apt install e2fsprogs"
        echo "  Fedora/RHEL:   sudo dnf install e2fsprogs"
        exit 1
    fi
fi

echo ""
echo "Created $DISK_IMG (${SIZE_MB}MB ext2)"
echo ""
echo "To add files to the disk image:"
if [ "$OS" = "Darwin" ]; then
    echo "  # Using debugfs (from e2fsprogs):"
    echo "  debugfs -w $DISK_IMG -R 'write yourfile.txt yourfile.txt'"
    echo "  debugfs $DISK_IMG -R 'ls'"
    echo ""
    echo "  # Or using fuse-ext2 (if installed):"
    echo "  mkdir -p mnt"
    echo "  fuse-ext2 $DISK_IMG mnt -o rw+"
    echo "  cp yourfile.txt mnt/"
    echo "  umount mnt"
else
    echo "  mkdir -p mnt"
    echo "  sudo mount -o loop $DISK_IMG mnt"
    echo "  sudo cp yourfile.txt mnt/"
    echo "  sudo umount mnt"
fi
echo ""
echo "To copy bootstrap files:"
if [ "$OS" = "Darwin" ]; then
    echo "  # Copy public folder contents"
    echo "  for f in bootstrap/public/*; do"
    echo "    debugfs -w $DISK_IMG -R \"write \$f \$(basename \$f)\""
    echo "  done"
else
    echo "  mkdir -p mnt"
    echo "  sudo mount -o loop $DISK_IMG mnt"
    echo "  sudo cp -r bootstrap/public/* mnt/"
    echo "  sudo umount mnt"
fi
