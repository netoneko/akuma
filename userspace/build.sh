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
    "tcc"
)

for member in "${MEMBERS[@]}"; do
    echo "Building $member..."
    cargo build --release -p "$member"
    # Special handling for tcc to copy its sysroot archive
    if [ "$member" == "tcc" ]; then
        TCC_SYSROOT_ARCHIVE="tcc/dist/tcc_sysroot.tar.gz"
        if [ -f "$TCC_SYSROOT_ARCHIVE" ]; then
            mkdir -p ../bootstrap/lib/tcc/
            cp "$TCC_SYSROOT_ARCHIVE" ../bootstrap/lib/tcc/
            echo "Copied $TCC_SYSROOT_ARCHIVE to ../bootstrap/lib/tcc/"
        else
            echo "Warning: TCC sysroot archive not found at $TCC_SYSROOT_ARCHIVE"
        fi
    fi
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
	elif [ "$bin" == "tcc" ] && [ -f "target/aarch64-unknown-none/release/cc" ]; then
            cp "target/aarch64-unknown-none/release/tcc" ../bootstrap/bin/
        else
            echo "Warning: Binary $bin not found at $SRC"
        fi
    fi
done

# Copy hello world example
cp tcc/examples/hello_world/hello.c ../bootstrap/hello.c

# Link sh to paws
PAWS_BIN="../bootstrap/bin/paws"
if [ -f "$PAWS_BIN" ]; then
    cp "$PAWS_BIN" "../bootstrap/bin/sh"
fi

echo "Build process completed."
