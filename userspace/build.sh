#!/bin/bash
set -e

# meow ships size-optimized: rebuild core/alloc from source with the
# immediate-abort panic strategy. This drops the residual panic plumbing the
# precompiled core carries (panic_fmt landing pads, location records), saving a
# full page off the binary. Costs a one-time core/alloc recompile (~8s). The
# trade-off: panics trap immediately instead of printing via the panic handler.
MEOW_SIZE_FLAGS=(
    -Z build-std=core,alloc
    --config 'target.aarch64-unknown-none.rustflags=["-Zunstable-options","-Cpanic=immediate-abort","-Crelocation-model=static"]'
)

# Build one workspace member, applying meow's size flags when appropriate.
build_member() {
    local m="$1"
    if [ "$m" == "meow" ]; then
        cargo build --release -p meow "${MEOW_SIZE_FLAGS[@]}"
    else
        cargo build --release -p "$m"
    fi
}

WITH_FORKTEST=false
MEMBER_ONLY=""
for arg in "$@"; do
    case "$arg" in
        --with-forktest) WITH_FORKTEST=true ;;
        --*-only)
            member="${arg#--}"
            MEMBER_ONLY="${member%-only}"
            ;;
    esac
done

if [ -n "$MEMBER_ONLY" ]; then
    echo "Building $MEMBER_ONLY only..."
    build_member "$MEMBER_ONLY"
    if [ "$MEMBER_ONLY" == "tcc" ]; then
        LIBC_ARCHIVE="tcc/dist/libc.tar"
        if [ -f "$LIBC_ARCHIVE" ]; then
            mkdir -p ../bootstrap/archives/
            cp "$LIBC_ARCHIVE" ../bootstrap/archives/libc.tar
        fi
        LIBTCC1_ARCHIVE="tcc/dist/libtcc1.tar"
        if [ -f "$LIBTCC1_ARCHIVE" ]; then
            mkdir -p ../bootstrap/archives/
            cp "$LIBTCC1_ARCHIVE" ../bootstrap/archives/libtcc1.tar
        fi
    fi
    # Members that produce no binary (build output handled by their build script)
    NO_BIN_MEMBERS=("apk-tools" "libakuma" "libakuma-tls" "crush")
    is_no_bin=false
    for m in "${NO_BIN_MEMBERS[@]}"; do
        [ "$MEMBER_ONLY" == "$m" ] && is_no_bin=true && break
    done
    if [ "$is_no_bin" = false ]; then
        mkdir -p ../bootstrap/bin
        if [ "$MEMBER_ONLY" == "quickjs" ] && [ -f "target/aarch64-unknown-none/release/qjs" ]; then
            cp "target/aarch64-unknown-none/release/qjs" ../bootstrap/bin/
        elif [ -f "target/aarch64-unknown-none/release/$MEMBER_ONLY" ]; then
            cp "target/aarch64-unknown-none/release/$MEMBER_ONLY" ../bootstrap/bin/
        else
            echo "Warning: Binary $MEMBER_ONLY not found"
        fi
    fi
    echo "Build process completed."
    exit 0
fi

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
    "stackstress"
    "stdcheck"
    "termtest"
    "allocstress"
    "top"
    "box"
    "tcc"
    "tar"
    "sshd"
    "llama-cpp"
    "crush"
    "stp_test"
    )


for member in "${MEMBERS[@]}"; do
    echo "Building $member..."
    build_member "$member"
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
        LIBTCC1_ARCHIVE="tcc/dist/libtcc1.tar"
        if [ -f "$LIBTCC1_ARCHIVE" ]; then
            mkdir -p ../bootstrap/archives/
            cp "$LIBTCC1_ARCHIVE" ../bootstrap/archives/libtcc1.tar
            echo "Copied $LIBTCC1_ARCHIVE to ../bootstrap/archives/libtcc1.tar"
        else
            echo "Warning: libtcc1 archive not found at $LIBTCC1_ARCHIVE"
        fi
    fi
done

# Create bin directory if it doesn't exist
mkdir -p ../bootstrap/bin

# Copy binaries (only if they exist)
BINARIES=(
    "hello"
    "echo2"
    "stackstress"
    "stdcheck"
    "elftest"
    "httpd"
    "meow"
    "quickjs"
    "herd"
    "termtest"
    "allocstress"
    "top"
    "box"
    "tcc"
    "tar"
    "sshd"
    "llama-cli"
    "crush"
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

# Build forktest (Go, opt-in via --with-forktest)
if [ "$WITH_FORKTEST" = true ]; then
    echo "Building forktest (Go)..."
    (
        cd forktest
        GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o forktest_child ./child
        GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o forktest_parent ./parent
        # Output must not equal the package dir name (./pattern2_minimal is a directory).
        GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o pattern2_minimal.bin ./pattern2_minimal
    )
    cp forktest/forktest_child ../bootstrap/bin/
    cp forktest/forktest_parent ../bootstrap/bin/
    cp forktest/pattern2_minimal.bin ../bootstrap/bin/pattern2_minimal
    echo "forktest binaries copied to bootstrap/bin/"

    # C-only mmap stress control: pure musl static binary, no Go runtime.
    # Used to disambiguate kernel-vs-runtime crashes (see
    # docs/GO_FORKTEST_DEBUG.md and the forktest_parent --use_c_child flag).
    echo "Building forktest mmap_stress (C control)..."
    (
        cd forktest/c_stress
        aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o mmap_stress mmap_stress.c
        aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o pattern2_parent pattern2_parent.c
    )
    cp forktest/c_stress/mmap_stress ../bootstrap/bin/
    cp forktest/c_stress/pattern2_parent ../bootstrap/bin/
    echo "mmap_stress + pattern2_parent (C) copied to bootstrap/bin/"
fi

echo "Build process completed."
