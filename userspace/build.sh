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
    # --force-rebuild: wipe this member's artifacts first so its build script
    # re-runs. Needed for members whose build.rs drives an external build (e.g.
    # llama-cpp's CMake) and only declares `rerun-if-changed=build.rs`, so edits
    # to the vendored C/C++ sources are otherwise not picked up by cargo.
    if [ "$FORCE_REBUILD" = true ]; then
        echo "Force-rebuilding $m (cargo clean -p $m)..."
        cargo clean --release -p "$m"
    fi
    if [ "$m" == "meow" ]; then
        cargo build --release -p meow "${MEOW_SIZE_FLAGS[@]}"
    else
        cargo build --release -p "$m"
    fi
}

WITH_FORKTEST=false
FORCE_REBUILD=false
MEMBER_ONLY=""
for arg in "$@"; do
    case "$arg" in
        --with-forktest) WITH_FORKTEST=true ;;
        --force-rebuild) FORCE_REBUILD=true ;;
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
        # tcc ships only libtcc1.tar (libtcc1.a + tcc's internal headers). The
        # musl sysroot is NOT shipped — install it on Akuma with `apk add musl-dev`.
        LIBTCC1_ARCHIVE="tcc/dist/libtcc1.tar"
        if [ -f "$LIBTCC1_ARCHIVE" ]; then
            mkdir -p ../bootstrap/archives/
            cp "$LIBTCC1_ARCHIVE" ../bootstrap/archives/libtcc1.tar
        fi
    fi
    # Members that produce no binary (build output handled by their build script)
    # nca's build.rs deploys the binary directly to bootstrap/bin/nca
    NO_BIN_MEMBERS=("apk-tools" "libakuma" "libakuma-tls" "crush" "nca")
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
    "wavplay"
    "scratch"
    "needle-server"
    "nca"
    )


for member in "${MEMBERS[@]}"; do
    echo "Building $member..."
    build_member "$member"
    # Special handling for tcc to copy its runtime archive. Only libtcc1.tar is
    # shipped (libtcc1.a + tcc internal headers); musl comes from `apk add
    # musl-dev` on Akuma, so there is no in-tree musl sysroot / libc.tar anymore.
    if [ "$member" == "tcc" ]; then
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
    "needle-server"
    "nca"
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

# Build mmap_file: pure-C, static musl. Used by the kernel boot self-test
# (test_mmap_file_oom_survives) to prove a file-backed mmap larger than RAM
# SIGSEGVs the process instead of panicking the kernel
# (docs/LLAMA_MMAP_OOM_KERNEL_ABORT.md). Built unconditionally — it is tiny and
# the boot suite runs on every build.
echo "Building mmap_file (C, file-backed mmap OOM probe)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o mmap_file mmap_file.c
)
cp forktest/c_stress/mmap_file ../bootstrap/bin/
echo "mmap_file (C) copied to bootstrap/bin/"

# mprotectlb + clonearg: deterministic thread-spawn / mprotect probes. Both are
# one-shot (no stress loop) and calibrated against real Linux, so a FAIL is a
# kernel divergence and nothing else. mprotectlb is the regression guard for the
# 2026-08-05 `flush_tlb_range` ASID bug (mprotect could not downgrade a cached
# translation); clonearg checks that a cloned thread sees the memory its parent
# wrote just before the clone. Tiny; built unconditionally.
echo "Building mprotectlb + clonearg (C, thread-spawn/mprotect probes)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o mprotectlb mprotectlb.c
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -fno-stack-protector -o clonearg clonearg.c
)
cp forktest/c_stress/mprotectlb ../bootstrap/bin/
cp forktest/c_stress/clonearg ../bootstrap/bin/
echo "mprotectlb + clonearg (C) copied to bootstrap/bin/"

# spawnalias: the address-space identity canary for the thread-spawn SIGSEGV
# class. Unlike clonearg (which proved the clone *handoff* is sound and would
# pass regardless) this asks whose memory a freshly-spawned thread is actually
# reading — the nonce is a function of the pid, so a wrong value names the
# process it leaked from. Stress-shaped, not one-shot; calibrated on Linux.
# docs/runbooks/debug-thread-spawn-segv.md §3c. Tiny; built unconditionally.
echo "Building spawnalias (C, address-space identity canary)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o spawnalias spawnalias.c
)
cp forktest/c_stress/spawnalias ../bootstrap/bin/
echo "spawnalias (C) copied to bootstrap/bin/"

# tidflags: deterministic probe of clone(2)'s three tid flags. Regression guard
# for the 2026-08-06 bug where clone_thread wrote the child TID into the
# CLONE_CHILD_CLEARTID pointer at clone time — for musl that word is
# &__thread_list_lock, so every thread spawn stamped a live tid into the
# thread-list mutex and __tl_lock's "val == tid" fast path handed the lock to
# the new child. One clone per check, no stress loop; 4 FAIL before, 8 PASS
# after, 8 PASS on Linux. Tiny; built unconditionally.
echo "Building tidflags (C, clone tid-flag semantics probe)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -fno-stack-protector -o tidflags tidflags.c
)
cp forktest/c_stress/tidflags ../bootstrap/bin/
echo "tidflags (C) copied to bootstrap/bin/"

# pthread_kill_eintr: does a pthread_kill signal interrupt a blocking read?
# Shaped after jobserver-rs's Helper::join (the path every rustc that reaches
# codegen runs). Also asserts an SA_RESTART handler does NOT interrupt, which
# is what keeps Go's SIGURG preemption working. Tiny; built unconditionally.
echo "Building pthread_kill_eintr (C, pthread_kill EINTR probe)..."
(
    cd eintr_repro
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o pthread_kill_eintr pthread_kill_eintr.c
)
cp eintr_repro/pthread_kill_eintr ../bootstrap/bin/
echo "pthread_kill_eintr (C) copied to bootstrap/bin/"

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
