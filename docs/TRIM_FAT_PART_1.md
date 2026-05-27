# Trim the Fat — Part 1

## Summary

Removed `userspace/xbps/` and restructured `userspace/apk-tools/` as the first
step in the infrastructure optimization described in
[`proposals/TRIM_SOME_FAT.md`](../proposals/TRIM_SOME_FAT.md).

The acceptance criterion — `apk add busybox` running to completion inside the
VM — has been verified and passes.

## What Was Done

### Removed

| Component | Reason |
|-----------|--------|
| `userspace/xbps/` | Void Linux package manager — redundant, complex vendor tree, superseded by the Alpine/apk model |
| `bootstrap/archives/xbps.tar` | Generated artifact; deleted with the source |

### Changed

| Component | Change |
|-----------|--------|
| `userspace/apk-tools/` | Restored and updated (3.0.5 → 3.0.6). No longer deleted — used via `--apk-only` to stage bootstrap assets without a full userspace build |
| `userspace/build.sh` | Added `--apk-only` flag: builds only `apk-tools`, exits early |
| `src/shell/commands/net.rs` (curl) | Added `-o <file>` output flag |
| `src/process_tests.rs` | Tests that require binaries now skip with a warning when `FAIL_TESTS_IF_TEST_BINARY_MISSING = false` instead of panicking |

### Added

| Component | Purpose |
|-----------|---------|
| `bootstrap/etc/apk/` | Alpine signing keys, repo URLs, arch — pre-staged by `apk-tools` build.rs |
| `bootstrap/etc/resolv.conf` | DNS config for the VM |
| `bootstrap/etc/sshd/authorized_keys` | SSH public key (provisioned manually, see acceptance doc) |
| `acceptance/01_verify_apk_bootstrap.md` | Step-by-step acceptance procedure for fresh-image bootstrap |

## Bootstrap Model

`apk` and its signing keys are staged into `bootstrap/` at build time (not
downloaded at runtime). Fresh-image setup:

```
userspace/build.sh --apk-only   # download apk binary + keys + CA certs → bootstrap/
scripts/create_disk.sh          # create blank ext2 disk.img
scripts/populate_disk.sh        # copy bootstrap/ into disk.img via Docker
cargo run --release             # boot the VM
ssh -p 2222 root@localhost "apk add busybox"
```

## Acceptance

```
akuma:/> apk add busybox
(1/2) Installing musl (1.2.5-r23)
(2/2) Installing busybox (1.37.0-r30)
  Executing busybox-1.37.0-r30.post-install
Executing busybox-1.37.0-r30.trigger
OK: 1612 KiB in 2 packages
akuma:/> busybox sh
sh: can't access tty; job control turned off
~ # exit
```

Full procedure: [`acceptance/01_verify_apk_bootstrap.md`](../acceptance/01_verify_apk_bootstrap.md)

## What Can Now Be Eliminated (Part 2)

With busybox available via `apk add`, these in-tree components become redundant:

- **`userspace/sbase/`** — Unix utilities (ls, cat, echo, …) duplicated by busybox
- **`userspace/top/`** — replaced by `busybox top`
- **`userspace/cat/`** — replaced by `busybox cat`
- **`userspace/echo2/`** — replaced by `busybox echo`
- **`bootstrap/bin/sbase`** symlink farm and related disk space

Syscall requirements for `apk` and `busybox` are tracked in
`docs/APK_MISSING_SYSCALLS.md`.
