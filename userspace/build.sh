#!/bin/bash
set -e

# List of members to build (excluding those known to fail with std issues)
MEMBERS=(
    "libakuma"
    "libakuma-tls"
    "echo2"
    "elftest"
    "hello"
    "herd"
    "httpd"
    "meow"
    "quickjs"
    "scratch"
    "sqld"
    "stackstress"
    "stdcheck"
    "wget"
    "termtest"
    "allocstress"
    "top"
    "cat"
    "box"
    "paws"
)

for member in "${MEMBERS[@]}"; do
    echo "Building $member..."
    cargo build --release -p "$member"
done

# Create bin directory if it doesn't exist
mkdir -p ../bootstrap/bin

# Copy binaries (only if they exist)
BINARIES=(
    "hello"
    "cat"
    "echo2"
    "stackstress"
    "stdcheck"
    "elftest"
    "httpd"
    "meow"
    "wget"
    "sqld"
    "quickjs"
    "scratch"
    "herd"
    "chainlink"
    "termtest"
    "allocstress"
    "top"
    "box"
    "paws"
    "tcc"
)

for bin in "${BINARIES[@]}"; do
    SRC="target/aarch64-unknown-none/release/$bin"
    if [ -f "$SRC" ]; then
        cp "$SRC" ../bootstrap/bin/
    else
        # For quickjs the bin name might be qjs
        if [ "$bin" == "quickjs" ] && [ -f "target/aarch64-unknown-none/release/qjs" ]; then
            cp "target/aarch64-unknown-none/release/qjs" ../bootstrap/bin/
        else
            echo "Warning: Binary $bin not found at $SRC"
        fi
    fi
done

# Copy TCC headers and libs
mkdir -p ../bootstrap/usr/include
mkdir -p ../bootstrap/usr/include/sys
mkdir -p ../bootstrap/usr/lib
cp -r tcc/include/* ../bootstrap/usr/include/
cp tcc/lib/* ../bootstrap/usr/lib/
# Copy hello world example
cp tcc/examples/hello_world/hello.c ../bootstrap/hello.c

# Link sh to paws
PAWS_BIN="../bootstrap/bin/paws"
if [ -f "$PAWS_BIN" ]; then
    cp "$PAWS_BIN" "../bootstrap/bin/sh"
fi

echo "Build process completed."
