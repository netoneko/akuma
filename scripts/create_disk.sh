#!/bin/bash
# Create a FAT32 disk image for the Akuma kernel
# Usage: ./scripts/create_disk.sh [size_mb]

set -e

SIZE_MB=${1:-32}
DISK_IMG="disk.img"

echo "Creating ${SIZE_MB}MB FAT32 disk image..."

# Remove existing image if present
if [ -f "$DISK_IMG" ]; then
    echo "Removing existing $DISK_IMG"
    rm "$DISK_IMG"
fi

# Detect OS and use appropriate method
OS=$(uname -s)

if [ "$OS" = "Darwin" ]; then
    # macOS: Use mtools if available, otherwise provide instructions
    if command -v mformat &> /dev/null; then
        # Create empty disk image
        dd if=/dev/zero of="$DISK_IMG" bs=1m count="$SIZE_MB"
        # Format with mtools
        mformat -F -v AKUMA -i "$DISK_IMG" ::
    else
        echo "mtools not found. Installing via Homebrew or creating manually..."
        # Create a raw file and format it
        dd if=/dev/zero of="$DISK_IMG" bs=1m count="$SIZE_MB"
        
        # Try with a loop device (requires sudo)
        echo ""
        echo "The disk image has been created but needs to be formatted."
        echo "Please run one of the following:"
        echo ""
        echo "Option 1: Install mtools (recommended)"
        echo "  brew install mtools"
        echo "  mformat -F -v AKUMA -i $DISK_IMG ::"
        echo ""
        echo "Option 2: Use hdiutil (may require sudo)"
        echo "  hdiutil attach -nomount $DISK_IMG"
        echo "  # Note the /dev/diskN path"
        echo "  newfs_msdos -F 32 -v AKUMA /dev/diskN"
        echo "  hdiutil detach /dev/diskN"
        echo ""
        exit 0
    fi
else
    # Linux: Use mkfs.vfat or mkfs.fat
    # Create empty disk image
    dd if=/dev/zero of="$DISK_IMG" bs=1M count="$SIZE_MB" status=progress
    
    if command -v mkfs.vfat &> /dev/null; then
        mkfs.vfat -F 32 -n "AKUMA" "$DISK_IMG"
    elif command -v mkfs.fat &> /dev/null; then
        mkfs.fat -F 32 -n "AKUMA" "$DISK_IMG"
    else
        echo "Error: mkfs.vfat or mkfs.fat not found. Install dosfstools."
        echo "  Ubuntu/Debian: sudo apt install dosfstools"
        echo "  Fedora/RHEL:   sudo dnf install dosfstools"
        exit 1
    fi
fi

echo ""
echo "Created $DISK_IMG (${SIZE_MB}MB FAT32)"
echo ""
echo "To add files to the disk image:"
if [ "$OS" = "Darwin" ]; then
    echo "  # Using mtools:"
    echo "  mcopy -i $DISK_IMG yourfile.txt ::"
    echo "  mdir -i $DISK_IMG ::"
    echo ""
    echo "  # Or mount it:"
    echo "  hdiutil attach -imagekey diskimage-class=CRawDiskImage $DISK_IMG"
    echo "  # Copy files to /Volumes/AKUMA"
    echo "  hdiutil detach /Volumes/AKUMA"
else
    echo "  mkdir -p mnt"
    echo "  sudo mount -o loop $DISK_IMG mnt"
    echo "  sudo cp yourfile.txt mnt/"
    echo "  sudo umount mnt"
fi

