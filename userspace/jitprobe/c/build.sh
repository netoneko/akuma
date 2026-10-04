#!/bin/bash
# Build jit_probe (x86_64 musl static) and optionally push it to the box.
#   userspace/jitprobe/c/build.sh                 # build
#   userspace/jitprobe/c/build.sh --push-akuma    # + base64 over `ssh akuma` to /tmp
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p x86_64
x86_64-linux-musl-gcc -static -O2 -Wall -Wextra -o x86_64/jit_probe jit_probe.c
echo "built $PWD/x86_64/jit_probe ($(wc -c < x86_64/jit_probe) bytes)"
if [ "${1:-}" = "--push-akuma" ]; then
  base64 < x86_64/jit_probe | ssh akuma "base64 -d > /tmp/jit_probe && chmod +x /tmp/jit_probe && ls -l /tmp/jit_probe"
fi
