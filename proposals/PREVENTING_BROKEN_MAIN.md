# Preventing a broken main

**Status:** items 1-3, 5 (as `scripts/gate.py`), 6 and 7 done 2026-11-19; item 4 (CI) deliberately skipped. Source: `docs/archive/HOW_MAIN_BROKE_2026_11.md` — a type
refactor touched `akuma-syscalls-glue`, `akuma-exec` and `akuma-tokio-kernel-core`, and only the main
kernel was ever built; one behaviour change (`9cc41207`) broke the tokio kernels at runtime.

| # | Mitigation | Catches | Cost | State |
|---|---|---|---|---|
| 1 | Install `scripts/git-hooks/pre-commit` (`git config core.hooksPath scripts/git-hooks`) | clippy, host tests, ratchets, secret scan | minutes per commit | done in this clone |
| 2 | Hook runs `cargo check -p {taoxichuan,shanghai,denver} --target {aarch64,x86_64}-unknown-none` | signature drift in shared crates | seconds when warm | done |
| 3 | `scripts/install-hooks.sh` run by `userspace/build.sh` / `create_disk.sh`, plus a warning in `premerge_build.sh` when `core.hooksPath` is unset | the hook existing in the repo but not installed (it was never active here) | tiny | done: `scripts/install-hooks.sh`, warning in `premerge_build.sh` and `gate.py` |
| 4 | (skipped by decision) CI (GitHub Actions) running `premerge_build.sh --fast` on every push to a branch and the full set on PRs to main; branch protection requires it | everything a local hook skips (`--no-verify`, other clones) | CI minutes; use `headless` so the `spleen` submodule is not needed | todo |
| 5 | Script the tokio gate (`userspace/tokioprobe`, `epoch=`) and the Denver JVM gate into `scripts/`; run when a diff touches a shared crate (path filter on the 3 crates) | runtime regressions no build can see | needs KVM runner | done: `scripts/gate.py` boots every amd64 kernel on `gateprobe`; the Denver JVM gate is `--with-jvm` |
| 6 | A refactor that changes behaviour per architecture lists, in the commit message, every kernel that depends on the old behaviour (`git grep` the crate's dependents) | review-time blind spots | discipline | done: CLAUDE.md rule |
| 7 | A ratchet's "budget is 0" milestone requires `premerge_build.sh` to pass in the same commit | counting gates declared done without compiling every caller | one rule in CLAUDE.md | done: CLAUDE.md rule |

Why platform kernels are not in `default-members`: cargo unifies features across one resolve, and
adding them makes a bare `cargo check` fail in `akuma-kernel-tests-aarch64` (192 errors, measured
2026-11-19). Checking each kernel on its own avoids that. Fixing it properly means making the shared
crates' features additive.
