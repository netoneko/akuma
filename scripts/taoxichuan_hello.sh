#!/bin/sh
# Taoxichuan stage 2: build the gate's static musl hello on the host (amd64),
# run it, and save its exact output as the reference the guest is compared
# against. POSIX sh (the box's rootfs is busybox ash).
#
# Usage: scripts/taoxichuan_hello.sh [outdir]
#   outdir defaults to /root/dev-cycle/taoxichuan-s2
#
# -no-pie: the composed kernel's loader places ET_EXEC images at their link
# addresses (0x400000) and does not relocate; a static-PIE image would need a
# bias the minimal loader deliberately does not have.
set -e

OUT="${1:-/root/dev-cycle/taoxichuan-s2}"
CC=x86_64-alpine-linux-musl-gcc

mkdir -p "$OUT"
cat > "$OUT/hello.c" <<'EOF'
#include <stdio.h>
int main(void) { printf("hello from taoxichuan ring 3\n"); return 0; }
EOF

"$CC" -static -no-pie -O2 -o "$OUT/hello" "$OUT/hello.c"
"$OUT/hello" > "$OUT/reference.out"

# The guest console turns \n into \r\n (akuma-comconsole), so the byte-compare
# is done on \n-normalised content on both sides; keep the raw reference here.
echo "hello: $(ls -l "$OUT/hello" | awk '{print $5}') bytes; reference: $(wc -c < "$OUT/reference.out") bytes"
