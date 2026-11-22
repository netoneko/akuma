#!/bin/sh
# Point this clone (and every worktree of it) at the repo's hooks: scripts/git-hooks/pre-commit runs the
# console-print greps, clippy over every crate, `cargo check` of the platform kernels and the host tests.
# Idempotent. `core.hooksPath` is per repository config, so a fresh clone must run this once.
set -e
cd "$(dirname "$0")/.."
git config core.hooksPath scripts/git-hooks
echo "core.hooksPath = $(git config core.hooksPath)"
