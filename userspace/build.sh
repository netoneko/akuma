#!/bin/bash
set -e

# Every path below is relative to userspace/ (`../bootstrap/bin`, `tcc/dist`, …)
# and the nightly toolchain + aarch64-unknown-none target come from
# userspace/{rust-toolchain.toml,.cargo/config.toml}, which cargo resolves from
# the *working* directory. The documented invocation is `userspace/build.sh`
# from the repo root, so anchor there instead of trusting the caller's cwd.
cd "$(dirname "$0")"

# meow ships size-optimized: rebuild core/alloc from source (here) with the
# immediate-abort panic strategy (MEOW_RUSTFLAGS below). This drops the residual
# panic plumbing the precompiled core carries (panic_fmt landing pads, location
# records), saving a full page off the binary. Costs a one-time core/alloc
# recompile (~8s). The trade-off: panics trap immediately instead of printing
# via the panic handler.
MEOW_SIZE_FLAGS=(
    -Z build-std=core,alloc
)

# rustflags for the out-of-workspace members below, spelled out because they
# cannot inherit the config files the way a workspace member does.
#
# `.cargo/config.toml` in the repo root contributes `-Clink-arg=-Tlinker.ld` and
# userspace/.cargo/config.toml the rest (cargo merges the two arrays). The
# linker-script path is *relative*, and cargo runs rustc with the cwd set to the
# workspace root — userspace/, where linker.ld lives, for a member; but meow/ or
# tcc/ for a member that is its own workspace, where it does not exist, and the
# link fails with "cannot find linker script linker.ld". So make it absolute.
#
# CARGO_ENCODED_RUSTFLAGS (\x1f-separated, one element per argument) *replaces*
# `target.<triple>.rustflags` from every config file rather than merging with it,
# which is what keeps the relative -Tlinker.ld out.
EXTERNAL_RUSTFLAGS=(
    -C relocation-model=static
    -C link-arg=-z
    -C link-arg=max-page-size=0x1000
    -C "link-arg=-T$PWD/linker.ld"
)

# meow ships with the immediate-abort panic strategy (see MEOW_SIZE_FLAGS).
MEOW_RUSTFLAGS=(
    -Zunstable-options
    -C panic=immediate-abort
    "${EXTERNAL_RUSTFLAGS[@]}"
)

# Join arguments with \x1f for CARGO_ENCODED_RUSTFLAGS. Encoded rather than
# space-separated RUSTFLAGS so a path with a space cannot split an argument.
encode_rustflags() {
    local IFS=$'\x1f'
    echo "$*"
}

# Members that are NOT in the userspace workspace. The submodule-backed ones
# were dropped from `members` so a missing submodule cannot block building
# unrelated crates (see the note at the top of userspace/Cargo.toml) — which
# also means `cargo build -p <name>` no longer finds them. Each is built through
# its own manifest instead, and its artifacts land in its own target/ dir.
#
#   <name used by this script>|<dir>|<cargo package>|<binary>|<path that must exist>
#
# An empty <binary> means the member's build script installs into bootstrap/bin
# itself (llama-cpp installs llama-cli/-server/-bench, nca installs nca), so
# there is nothing here to copy. The last field is a file from the member's
# submodule: absent means the submodule is not checked out, and the member is
# skipped rather than failing the whole build.
EXTERNAL_MEMBERS=(
    "meow|meow|meow|meow|meow/Cargo.toml"
    "tcc|tcc|tcc|tcc|tcc/tinycc/libtcc.c"
    "llama-cpp|llama.cpp|llama-cpp||llama.cpp/llama.cpp/CMakeLists.txt"
    "nca|nca|native-cli-ai||nca/native-cli-ai/Cargo.toml"
)

# Print the EXTERNAL_MEMBERS row for $1; fail if $1 is an ordinary workspace member.
external_spec() {
    local row
    for row in "${EXTERNAL_MEMBERS[@]}"; do
        if [ "${row%%|*}" == "$1" ]; then
            echo "$row"
            return 0
        fi
    done
    return 1
}

# Print where $1's binary lands. Workspace members share userspace/target; an
# external member has its own target/ under its directory. Fails (prints
# nothing) for members whose build script does its own installing.
member_bin_path() {
    local spec dir bin
    if spec=$(external_spec "$1"); then
        IFS='|' read -r _ dir _ bin _ <<<"$spec"
        [ -z "$bin" ] && return 1
        echo "$dir/target/aarch64-unknown-none/release/$bin"
        return 0
    fi
    echo "target/aarch64-unknown-none/release/$1"
}

# Build one member, applying meow's size flags when appropriate.
build_member() {
    local m="$1" spec dir pkg req manifest
    if spec=$(external_spec "$m"); then
        IFS='|' read -r _ dir pkg _ req <<<"$spec"
        if [ ! -e "$req" ]; then
            echo "  (skipping $m: $req missing — submodule not checked out)"
            return 0
        fi
        manifest="$dir/Cargo.toml"
        if [ "$FORCE_REBUILD" = true ]; then
            echo "Force-rebuilding $m (cargo clean -p $pkg)..."
            cargo clean --release --manifest-path "$manifest" -p "$pkg"
        fi
        if [ "$m" == "meow" ]; then
            CARGO_ENCODED_RUSTFLAGS="$(encode_rustflags "${MEOW_RUSTFLAGS[@]}")" \
                cargo build --release --manifest-path "$manifest" "${MEOW_SIZE_FLAGS[@]}"
        else
            CARGO_ENCODED_RUSTFLAGS="$(encode_rustflags "${EXTERNAL_RUSTFLAGS[@]}")" \
                cargo build --release --manifest-path "$manifest"
        fi
        return 0
    fi
    # --force-rebuild: wipe this member's artifacts first so its build script
    # re-runs. Needed for members whose build.rs drives an external build (e.g.
    # llama-cpp's CMake) and only declares `rerun-if-changed=build.rs`, so edits
    # to the vendored C/C++ sources are otherwise not picked up by cargo.
    if [ "$FORCE_REBUILD" = true ]; then
        echo "Force-rebuilding $m (cargo clean -p $m)..."
        cargo clean --release -p "$m"
    fi
    if [ "$m" == "sshd" ] && [ "${SSHD_FORK_SESSIONS:-1}" = "0" ]; then
        # Opt OUT of process-per-session sshd back to the single-process
        # cooperative executor (userspace/sshd/Cargo.toml `fork-sessions`, on by
        # default). For memory-constrained images where a process per session is
        # the wrong trade — see docs/runbooks/build-extreme-size.md.
        #
        # --no-default-features drops `akuma` too, so re-add it: it is what
        # links libakuma, and the binary cannot build without it.
        echo "  (sshd: fork-sessions DISABLED via SSHD_FORK_SESSIONS=0 — cooperative executor)"
        cargo build --release -p sshd --no-default-features --features akuma
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
    # Members that produce no binary of their own: libraries, plus apk-tools
    # (its build script emits the apk bootstrap assets). The members whose build
    # script installs into bootstrap/bin itself are handled by member_bin_path,
    # which deliberately resolves to nothing for them.
    NO_BIN_MEMBERS=("apk-tools" "libakuma" "libakuma-tls" "akuma-ssh-crypto")
    is_no_bin=false
    for m in "${NO_BIN_MEMBERS[@]}"; do
        [ "$MEMBER_ONLY" == "$m" ] && is_no_bin=true && break
    done
    if [ "$is_no_bin" = false ] && SRC=$(member_bin_path "$MEMBER_ONLY"); then
        mkdir -p ../bootstrap/bin
        if [ -f "$SRC" ]; then
            cp "$SRC" ../bootstrap/bin/
        else
            echo "Warning: Binary $MEMBER_ONLY not found at $SRC"
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
    "forkprobe"
    "hello"
    "paws"
    "herd"
    "httpd"
    "meow"
    "stackstress"
    "termtest"
    "allocstress"
    "box"
    "tcc"
    "tar"
    "sshd"
    "llama-cpp"
    "wavplay"
    "scratch"
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

# Copy binaries (only if they exist). llama-cli/-server/-bench and nca are NOT
# listed: their build scripts install them into bootstrap/bin directly, and they
# never appear under any target/ dir this script looks in.
BINARIES=(
    "hello"
    "paws"
    "echo2"
    "stackstress"
    "elftest"
    "forkprobe"
    "httpd"
    "meow"
    "herd"
    "termtest"
    "allocstress"
    "box"
    "tcc"
    "tar"
    "sshd"
)

for bin in "${BINARIES[@]}"; do
    SRC=$(member_bin_path "$bin")
    if [ -f "$SRC" ]; then
        cp "$SRC" ../bootstrap/bin/
    else
        echo "Warning: Binary $bin not found at $SRC"
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
# madvshared: the deterministic probe for MADV_DONTNEED on a CoW-shared frame
# (docs/archive/CARGO_HEAP_NULL_RC.md theory 3 — the null-`Rc` mechanism). Replaces a
# ~1-in-5 crash during a full in-guest cargo build as the instrument for that
# question. Calibrated ALL PASS on real Linux arm64; a FAIL here is the kernel.
echo "Building mprotectlb + clonearg + cowstale + bssfork + madvshared (C, thread-spawn/mprotect/CoW probes)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o mprotectlb mprotectlb.c
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -fno-stack-protector -o clonearg clonearg.c
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o cowstale cowstale.c -pthread
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o bssfork bssfork.c -pthread
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o madvshared madvshared.c
)
cp forktest/c_stress/mprotectlb ../bootstrap/bin/
cp forktest/c_stress/clonearg ../bootstrap/bin/
cp forktest/c_stress/cowstale ../bootstrap/bin/
cp forktest/c_stress/bssfork ../bootstrap/bin/
cp forktest/c_stress/madvshared ../bootstrap/bin/
echo "mprotectlb + clonearg + cowstale + bssfork + madvshared (C) copied to bootstrap/bin/"

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

# smapsdirty: /proc/self/smaps presence + Shared_Dirty accounting, plus the
# MADV_FREE return value. Reproduces redis-server's ARM64-COW-BUG startup check
# (redis/src/syscheck.c `checkLinuxMadvFreeForkBug`) verbatim, which is why
# redis refuses to start: it reads /proc/self/smaps, and Akuma implements no
# /proc/<pid>/ files at all. Despite the name of the warning this is NOT a CoW
# bug — redis prints a different message for that. One-shot, calibrated against
# real Linux (4 PASS there). docs/archive/LONG_ROAD_TO_REDIS.md. Tiny; built
# unconditionally.
echo "Building smapsdirty (C, /proc/self/smaps + MADV_FREE probe)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o smapsdirty smapsdirty.c
)
cp forktest/c_stress/smapsdirty ../bootstrap/bin/
echo "smapsdirty (C) copied to bootstrap/bin/"

# pthread_kill_eintr: does a pthread_kill signal interrupt a blocking read?
# Shaped after jobserver-rs's Helper::join (the path every rustc that reaches
# codegen runs). Also asserts an SA_RESTART handler does NOT interrupt, which
# is what keeps Go's SIGURG preemption working. Tiny; built unconditionally.
echo "Building pthread_kill_eintr (C, pthread_kill EINTR probe)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o pthread_kill_eintr pthread_kill_eintr.c
)
cp forktest/c_stress/pthread_kill_eintr ../bootstrap/bin/
echo "pthread_kill_eintr (C) copied to bootstrap/bin/"

# eager_mprotect_probe: does mprotect still hold on an EAGER mmap after the
# Failure-A recovery path (MmapRegion::flags + [EAGER-UPGRADE])? Guards against
# the upgrade gate firing when mprotect downgraded the region to read-only or
# PROT_NONE, which would silently defeat mprotect. Tiny; built unconditionally.
echo "Building eager_mprotect_probe (C, mprotect-vs-eager-region probe)..."
(
    cd forktest/c_stress
    aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o eager_mprotect_probe eager_mprotect_probe.c
)
cp forktest/c_stress/eager_mprotect_probe ../bootstrap/bin/
echo "eager_mprotect_probe (C) copied to bootstrap/bin/"

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
