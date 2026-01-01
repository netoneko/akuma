#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DOCKER_DIR="$SCRIPT_DIR/docker"

# Copy kernel to docker context
cp "$PROJECT_ROOT/target/aarch64-unknown-none/release/akuma" "$DOCKER_DIR/akuma"

# Build for arm64
docker build --platform linux/arm64 -t akuma-qemu "$DOCKER_DIR"

# Cleanup
rm "$DOCKER_DIR/akuma"

echo "Image built: akuma-qemu"
echo "Run with: docker run -it --platform linux/arm64 -p 2222:22 -p 8080:8080 -v /path/to/disk.img:/data/disk.img akuma-qemu"

