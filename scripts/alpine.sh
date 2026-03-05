#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

exec docker run --rm -it \
  --platform linux/arm64 \
  -v "$PROJECT_ROOT:/akuma" \
  -w /akuma \
  alpine:latest \
  sh
