#!/bin/bash
set -e
cargo build --release

# Create bin directory if it doesn't exist
mkdir -p ../bootstrap/bin

# Copy binaries
cp target/aarch64-unknown-none/release/hello ../bootstrap/bin/
cp target/aarch64-unknown-none/release/echo2 ../bootstrap/bin/
cp target/aarch64-unknown-none/release/stackstress ../bootstrap/bin/
cp target/aarch64-unknown-none/release/stdcheck ../bootstrap/bin/
cp target/aarch64-unknown-none/release/elftest ../bootstrap/bin/
cp target/aarch64-unknown-none/release/httpd ../bootstrap/bin/
cp target/aarch64-unknown-none/release/wget ../bootstrap/bin/
cp target/aarch64-unknown-none/release/sqld ../bootstrap/bin/
cp target/aarch64-unknown-none/release/qjs ../bootstrap/bin/

echo "Built and copied: hello, echo2, stackstress, stdcheck, elftest, httpd, wget, sqld, qjs"
