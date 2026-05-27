# Trim the Fat — Part 1: Removing xbps and apk-tools

## Summary

Removed the two bundled package managers (`userspace/xbps/` and `userspace/apk-tools/`) as the first step in the infrastructure optimization described in [`proposals/TRIM_SOME_FAT.md`](../proposals/TRIM_SOME_FAT.md).

## What Was Removed

| Component | Reason |
|-----------|--------|
| `userspace/xbps/` | Void Linux package manager — redundant, complex vendor tree, replaced by the Alpine/busybox model |
| `userspace/apk-tools/` | Pre-bundled Alpine apk binary — no longer baked in; bootstrapped on demand from Alpine CDN at runtime |
| `bootstrap/archives/xbps.tar` | Generated artifact from xbps build; deleted alongside the source |
| `bootstrap/archives/apk-tools.tar` | Generated artifact from apk-tools build; deleted alongside the source |

## What Replaced Them

The OS no longer ships a package manager as a pre-installed binary. Instead:

1. The built-in kernel `curl` command (HTTP/HTTPS GET, `src/shell/commands/net.rs`) is used to download `apk.static` from Alpine CDN at first boot or on demand.
2. `apk.static` is a statically-linked Alpine binary that needs no dynamic libraries and can bootstrap a full Alpine root from scratch.
3. `busybox` is then installed via `apk add busybox`, providing the full Unix utility suite.

This matches the model described in `proposals/TRIM_SOME_FAT.md`: success is defined by the stability of the syscalls required by `apk` and `busybox`, not by bundling every tool.

## How to Verify

Follow the steps in [`acceptance/01_verify_apk_bootstrap.md`](../acceptance/01_verify_apk_bootstrap.md) against a live Akuma QEMU instance.

## Syscall Requirements

For `apk` and `busybox` to work, the kernel must correctly implement the syscalls documented in `docs/APK_MISSING_SYSCALLS.md`. That file remains the reference for any syscall gaps discovered during bootstrap.

## Next Steps (Part 2)

Per the proposal, the next candidates for removal are:
- `sbase/` utilities that duplicate busybox functionality (low risk)
- `userspace/top/` (replace with `busybox top`)
