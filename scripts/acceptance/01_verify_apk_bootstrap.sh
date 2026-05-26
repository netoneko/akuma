#!/bin/bash
# Verify that the Alpine apk bootstrap flow works on a running Akuma instance.
# Requires QEMU/Akuma to be running with SSH on localhost:2222.
#
# Usage: ./scripts/verify_apk_bootstrap.sh [--host HOST] [--port PORT]

set -euo pipefail

HOST="${HOST:-localhost}"
PORT="${PORT:-2222}"
SSH_OPTS="-o StrictHostKeyChecking=no -o ConnectTimeout=5 -o BatchMode=yes"
SSH="ssh $SSH_OPTS -p $PORT root@$HOST"

PASS=0
FAIL=0
START_TIME=$(date +%s)

step() {
    local name="$1"
    local cmd="$2"
    local t0
    t0=$(date +%s%3N)
    if $SSH "$cmd" 2>&1; then
        local t1
        t1=$(date +%s%3N)
        echo "[PASS] $name ($((t1 - t0))ms)"
        PASS=$((PASS + 1))
    else
        local t1
        t1=$(date +%s%3N)
        echo "[FAIL] $name ($((t1 - t0))ms)"
        FAIL=$((FAIL + 1))
        echo "Aborting: step '$name' failed."
        exit 1
    fi
}

# Preflight: check SSH connectivity
echo "Checking SSH connectivity to $HOST:$PORT..."
if ! $SSH "echo connected" >/dev/null 2>&1; then
    echo ""
    echo "ERROR: Cannot connect to Akuma via SSH at $HOST:$PORT"
    echo ""
    echo "Start QEMU first:"
    echo "  cargo run --release"
    echo "  # or: ./scripts/run.sh"
    echo ""
    echo "Then re-run this script."
    exit 1
fi
echo "Connected."
echo ""

# Step 1: Download apk-tools-static from Alpine CDN
step "download apk-tools-static" \
    "curl -L 'https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/apk-tools-static-2.14.4-r1.apk' > /tmp/apk.apk"

# Step 2: Extract the static apk binary
step "extract apk.static" \
    "mkdir -p /tmp/apkstatic && tar xzf /tmp/apk.apk -C /tmp/apkstatic"

# Step 3: Initialize Alpine root db
step "apk --initdb" \
    "mkdir -p /tmp/apkroot && /tmp/apkstatic/sbin/apk.static --root /tmp/apkroot --initdb"

# Step 4: Set up Alpine repositories
step "write repositories" \
    "mkdir -p /tmp/apkroot/etc/apk && echo 'https://dl-cdn.alpinelinux.org/alpine/latest-stable/main' > /tmp/apkroot/etc/apk/repositories"

# Step 5: Install busybox
step "apk add busybox" \
    "/tmp/apkstatic/sbin/apk.static --root /tmp/apkroot add busybox"

# Step 6: Busybox sanity checks
step "busybox ls /" \
    "/tmp/apkroot/usr/bin/busybox ls /"

step "busybox echo" \
    "/tmp/apkroot/usr/bin/busybox echo 'busybox OK'"

step "busybox uname" \
    "/tmp/apkroot/usr/bin/busybox uname -a"

# Step 7: Install a second package to confirm the pkg DB is healthy
step "apk add file" \
    "/tmp/apkstatic/sbin/apk.static --root /tmp/apkroot add file"

step "file /bin/busybox" \
    "/tmp/apkroot/usr/bin/file /tmp/apkroot/bin/busybox"

END_TIME=$(date +%s)
echo ""
echo "Results: $PASS passed, $FAIL failed (total $((END_TIME - START_TIME))s)"

if [ "$FAIL" -eq 0 ]; then
    echo "All checks passed."
    exit 0
else
    exit 1
fi
