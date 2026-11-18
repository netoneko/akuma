#!/bin/bash
# Build every kernel target before merging to main.
#
#   scripts/premerge_build.sh                 # all of them, keep going past a failure, summary at the end
#   scripts/premerge_build.sh --list          # the names, one per line
#   scripts/premerge_build.sh --only denver   # steps whose name contains any of the comma-separated words
#   scripts/premerge_build.sh --fast          # skip the second feature set of each kernel (one build per kernel/arch)
#
# Exit status 0 only if every step built. Logs: $LOGDIR (default /tmp/premerge-build-<head>/<step>.log); a failed step
# prints the first errors of its log in the summary.
#
# **Why it exists.** The platform kernels (`taoxichuan`, `shanghai`, `denver`) share crates with the main kernel
# (`akuma-tokio-kernel-core`, `akuma-syscalls-glue`, `akuma-exec`) but are not in `default-members`, so `cargo build`
# at the root never reaches them: an API change in a shared crate broke `denver` on both architectures and nobody saw it
# until a clean-worktree build (`docs/archive/DENVER_BUILD_BREAKAGE.md`). A merge to main passes this first.
#
# What it builds is the set of *build targets* CLAUDE.md names (profile + feature set), `--release`:
#   akuma   aarch64: default; devbox (smoltcp: `no-tests`); rump devbox
#   akuma   amd64:   headless; headless,no-tests; the ryzen set (`no-tests,vfat,ext4` over the default drivers); server
#   taoxichuan, shanghai (+ its middleware sets), denver (default + the JVM overlay's feature set): amd64 and aarch64
# It does not run tests, clippy, the userspace build or any image: `cargo test` on the host crates and
# `scripts/check_*.py` are separate gates.
#
# Keep the feature lists in step with: scripts/build_devbox*.sh, overlays/ryzen/build.sh, overlays/denver/build-kernel.sh.
cd "$(dirname "$0")/.." || exit 2

A64=aarch64-unknown-none
X64=x86_64-unknown-none
# Denver's JVM gate sets (overlays/denver/build-kernel.sh).
DENVER_X64=wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal,shell
DENVER_A64=shell,trace-err,wx-relaxed,getcpu

# name | fast(1 = kept by --fast) | cargo arguments after `cargo build --release`
STEPS=(
  "akuma-aarch64|1|-p akuma --target $A64"
  "akuma-aarch64-devbox|0|-p akuma --target $A64 --features no-tests"
  "akuma-aarch64-devbox-rump|0|-p akuma --target $A64 --no-default-features --features rump-default,sc-reboot,sound,no-tests,linux-extra-syscalls"
  "akuma-amd64-headless|1|-p akuma --target $X64 --no-default-features --features headless"
  "akuma-amd64-headless-no-tests|0|-p akuma --target $X64 --no-default-features --features headless,no-tests"
  "akuma-amd64-ryzen|1|-p akuma --target $X64 --features no-tests,vfat,ext4"
  "akuma-amd64-server|0|-p akuma --target $X64 --no-default-features --features server"
  "taoxichuan-amd64|1|-p taoxichuan --target $X64 --features no-tests"
  "taoxichuan-aarch64|1|-p taoxichuan --target $A64 --features no-tests"
  "shanghai-amd64|1|-p shanghai --target $X64"
  "shanghai-amd64-middleware|0|-p shanghai --target $X64 --features middleware,no-dynamic-loading,signed-exec"
  "shanghai-aarch64|1|-p shanghai --target $A64"
  "denver-amd64|0|-p denver --target $X64"
  "denver-amd64-jvm|1|-p denver --target $X64 --features $DENVER_X64"
  "denver-aarch64|0|-p denver --target $A64"
  "denver-aarch64-jvm|1|-p denver --target $A64 --features $DENVER_A64"
)

only="" fast=0 list=0
while [ $# -gt 0 ]; do
  case "$1" in
    --only) only="$2"; shift ;;
    --fast) fast=1 ;;
    --list) list=1 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done

if [ "$list" = 1 ]; then
  for s in "${STEPS[@]}"; do echo "${s%%|*}"; done
  exit 0
fi

head=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
LOGDIR=${LOGDIR:-/tmp/premerge-build-$head}
mkdir -p "$LOGDIR"
dirty=$(git status --porcelain --untracked-files=no 2>/dev/null | wc -l | tr -d ' ')
echo "premerge_build: $head ($dirty modified tracked files), logs in $LOGDIR"
[ "$dirty" != 0 ] && echo "  note: the tree is dirty; this builds what is on disk, not what is committed"

selected() { # selected <name> <fast>
  if [ -n "$only" ]; then
    local w; for w in ${only//,/ }; do case "$1" in *"$w"*) return 0 ;; esac; done
    return 1
  fi
  [ "$fast" = 1 ] && [ "$2" != 1 ] && return 1
  return 0
}

pass=() fail=()
t0=$SECONDS
for s in "${STEPS[@]}"; do
  IFS='|' read -r name keep args <<<"$s"
  selected "$name" "$keep" || continue
  log=$LOGDIR/$name.log
  t=$SECONDS
  printf '%-32s ' "$name"
  # shellcheck disable=SC2086  # the argument string is word-split on purpose
  if cargo build --release $args >"$log" 2>&1; then
    echo "PASS ($((SECONDS - t))s)"
    pass+=("$name")
  else
    echo "FAIL ($((SECONDS - t))s)  $log"
    fail+=("$name")
  fi
done

echo
echo "premerge_build: ${#pass[@]} passed, ${#fail[@]} failed in $((SECONDS - t0))s"
for name in "${fail[@]}"; do
  echo "--- $name: $(grep -c '^error' "$LOGDIR/$name.log") error(s); first ones:"
  grep -E '^error' -A4 "$LOGDIR/$name.log" | head -14 | sed 's/^/    /'
done
[ "${#fail[@]}" = 0 ]
