#!/bin/sh
# `-C linker=` wrapper for the kernel target, wired in .cargo/config.toml.
#
# Purpose: cargo has no post-build hook — build.rs runs BEFORE its own crate is
# compiled and linked, so nothing in cargo can post-process the final binary.
# The link step is the one place that runs after the ELF exists and still inside
# `cargo build`, so that is where the flat-binary conversion goes. Without this,
# `akuma.bin` was produced only by scripts/cargo_runner.sh, i.e. only on
# `cargo run` — and a `cargo build --release` would leave the PREVIOUS build's
# .bin sitting there looking current. That bit a real ext2 A/B on 2026-08-26:
# the "new" arm was measured against a kernel image from the other arm.
#
# POSIX sh, no bashisms: this also runs INSIDE the guest during a self-hosted
# kernel build (acceptance/10), whose rootfs has busybox /bin/sh and no bash.
# A `#!/bin/bash` here would fail to exec and take the whole in-guest build
# down with it.
#
# This script is in the link path for EVERY binary built for
# aarch64-unknown-none in this workspace (in practice: just `akuma` — rlibs are
# archived, not linked, and build scripts link for the HOST target). It must
# therefore be conservative: link first, and never fail the build for anything
# other than a genuine link failure — hence the `|| true` on the .bin step. A
# guest without any objcopy still gets a working kernel ELF, just no .bin.
#
# `userspace/` is unaffected: cargo config is cwd-relative, and
# userspace/.cargo/config.toml sets no `linker`.
#
# RESIDUAL HOLE (why cargo_runner.sh still regenerates unconditionally): when
# cargo *uplifts* a cached artifact — you switch back to a feature set built
# earlier — it does not relink, so this script does not run and target/<profile>/
# akuma.bin still holds whatever the intervening build wrote. `cargo run` is
# safe because the runner objcopies every time; if you consume the .bin any
# other way after switching feature sets, verify it (size/mtime) or run
# scripts/mkbin.sh yourself.
set -e

HERE=$(dirname "$0")
LLD="${RUST_LLD:-rust-lld}"

# rustc invokes the lld-flavoured linker as `rust-lld -flavor gnu ...` and puts
# the sysroot's bin dir first on PATH, so a bare `rust-lld` resolves. Prepend
# the flavor defensively in case rustc ever stops passing it for a custom -C
# linker (rust-lld refuses to run without one).
if [ "${1:-}" != "-flavor" ]; then
  set -- -flavor gnu "$@"
fi

"$LLD" "$@"

# Locate the link output. rustc passes `-o <path>`; handle the joined form too.
OUT=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then OUT="$a"; break; fi
  case "$a" in
    -o?*) OUT="${a#-o}"; break ;;
  esac
  prev="$a"
done
[ -n "$OUT" ] || exit 0

# Only the kernel gets a flat image. The link target is the hash-suffixed
# artifact under deps/ (cargo uplifts it to <profile>/akuma afterwards), so
# convert from the file that exists NOW and write to where the uplifted ELF
# will land — that sidesteps the uplift ordering entirely.
case "$OUT" in
  */deps/akuma-*)
    profile_dir=$(dirname "$(dirname "$OUT")")
    base=$(basename "$OUT")
    "$HERE/mkbin.sh" "$OUT" "${profile_dir}/${base%-*}.bin" >/dev/null || true
    ;;
esac
exit 0
