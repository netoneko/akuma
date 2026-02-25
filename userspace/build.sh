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
    "pkg"
    "musl"
    "tcc"
    "tar"
    "make"
    "sbase"
    "sshd"
    "dash"
)

for member in "${MEMBERS[@]}"; do
    echo "Building $member..."
    cargo build --release -p "$member"
    # Special handling for tcc to copy its sysroot archive
    if [ "$member" == "tcc" ]; then
        LIBC_ARCHIVE="tcc/dist/libc.tar"
        if [ -f "$LIBC_ARCHIVE" ]; then
            mkdir -p ../bootstrap/archives/
            cp "$LIBC_ARCHIVE" ../bootstrap/archives/libc.tar
            echo "Copied $LIBC_ARCHIVE to ../bootstrap/archives/libc.tar"
        else
            echo "Warning: libc archive not found at $LIBC_ARCHIVE"
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
    "termtest"
    "allocstress"
    "top"
    "box"
    "paws"
    "pkg"
    "tcc"
    "tar"
    "sshd"
)

for bin in "${BINARIES[@]}"; do
    SRC="target/aarch64-unknown-none/release/$bin"
    if [ -f "$SRC" ]; then
        cp "$SRC" ../bootstrap/bin/
    else
        # For quickjs the bin name might be qjs
        if [ "$bin" == "quickjs" ] && [ -f "target/aarch64-unknown-none/release/qjs" ]; then
            cp "target/aarch64-unknown-none/release/qjs" ../bootstrap/bin/
	elif [ "$bin" == "tcc" ] && [ -f "target/aarch64-unknown-none/release/tcc" ]; then
            cp "target/aarch64-unknown-none/release/tcc" ../bootstrap/bin/tcc
        else
            echo "Warning: Binary $bin not found at $SRC"
        fi
    fi
done

# Copy hello world example
cp tcc/examples/hello_world/hello.c ../bootstrap/hello.c

# Link sh to dash
DASH_BIN="../bootstrap/bin/dash"
if [ -f "$DASH_BIN" ]; then
    cp "$DASH_BIN" "../bootstrap/bin/sh"
fi

echo "Build process completed."
