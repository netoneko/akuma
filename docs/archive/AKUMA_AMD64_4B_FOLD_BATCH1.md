# amd64 C1 step 4b, fold batch 1: the path-only `*at` family

**Date:** 2026-09-09
**Status:** landed. QEMU/TCG `SMP=1` **553/0**, `SMP=4` **563/0**;
Firecracker/KVM `SMP=1` **540/0**, `SMP=4` **550/0**; ring-3 `-n 30` **OK**
(`grandfork` ALL PASS); memory probes **8/10, 0 unexpected on both
transports**; clippy (`akuma-amd64`) clean; **`apk add file` installs and runs
end to end**.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `docs/archive/AKUMA_AMD64_4B_FLIP.md` — the refcount flip;
the per-fd-number `nonblock` and value-copy `dup` divergences it adopted are
in force here.
**Plan:** `proposals/NEXT_AGENT_AMD64_4B_VFS_FOLD.md` step 2, batch 1 of the
suggested leaf-first order.

`mkdirat`, `unlinkat`, `renameat`, `symlinkat` and `readlinkat` now dispatch
through `to_glue` into `akuma-syscalls-glue`'s `fs.rs`, and `fd.rs`'s five
implementations — plus `fd_path_debug` and `fs_err_str`, which only fed
`renameat`'s diagnostic print — are deleted. The legacy x86 shims (82/83/84/
87/88/89) hand glue the asm-generic number with `AT_FDCWD` in the dirfd slots.
The VFS underneath was already shared (`amd64/src/fs.rs` re-exports
`akuma_vfs_glue`), so the arms answer identically by construction; what
changed is who owns the code.

## Divergences the fold adopted (stated, not discovered)

- **`dirfd` resolution.** The local `resolve_at` pre-checked that the dirfd
  named a *directory* (one `metadata` per call) and answered `ENOTDIR`
  otherwise; glue's `dirfd_base` accepts any `File` descriptor and lets the
  VFS walk answer `ENOTDIR` for a non-directory base, and answers `EBADF` for
  a dirfd naming a non-file. Both are the tree's canonical shape now.
- **`readlinkat` is strictly more than the arm it replaced.** It
  distinguishes `EINVAL` (path exists, not a symlink) from `ENOENT` (path
  missing) — the local arm collapsed both to `ENOENT` — and serves
  `/proc/self/exe` from the registered image and non-file fd descriptions
  under `/proc/<pid>/fd`. Pinned by a new suite check that is **red by
  construction against the pre-fold kernel**: readlink of `/probe.txt` must
  be `EINVAL`, and the old arm's answer was `ENOENT`.
- **errno table.** Glue's `fs_error_to_errno` is the one table now; the one
  deliberate difference from the deleted `fs_err_errno` (`NotSupported` is
  `EIO` there, was `ENOSYS` here) persists until the `utimensat` fold, whose
  callers want `ENOSYS`, re-raises it.
- **`unlinkat` of a symlink** now also drops the in-kernel symlink-table
  entry (`remove_symlink`) before the ext2 `remove_file` — glue's ordering,
  which is what makes `rm` of a symlink not leave a zombie resolution behind.

## The gap the crate closed: `akuma_vfs_glue::fs::mark_initialized`

The first boot failed every folded arm with `EIO` — `mkdir`, `rmdir`, `mv`,
`rm`, including of plain files. Cause: glue's `fs.rs` arms reach the VFS
through `akuma_vfs_glue::fs::*`, whose every function sits behind
`is_initialized()` — and the only setter is `fs::init()`, the **AArch64 boot
path** (virtio-blk check, its own ext2/procfs mounts, the fpcache and
reap-hook wiring). amd64 brings its VFS up by a different route
(`fs::init_vfs` + `mount_root_on`: virtio-blk, USB disk, or RAM image) and
correctly never called that. So `FS_INITIALIZED` was permanently false and
every folded arm answered `NotInitialized`, flattened to `EIO` — a working
filesystem reporting a hardware fault.

The fix is in the crate: `akuma_vfs_glue::fs::mark_initialized()`, called by
`mount_root_on` once its mounts are live. A target that reaches the same
state by its own route states that, rather than importing a boot sequence it
cannot run.

## The bug the fold flushed out: `sys_setsockopt`'s raw user read

With the errno cause fixed, `apk update` died in ring 0:
`#PF err=1, cr2=0x7fffffffc58c`, rip inside `sock::sys_setsockopt`. The
option value lives on the caller's **stack**, and the arm read it with a raw
`read_volatile` — a `CR4.SMAP` violation the moment a rig enables SMAP. It
is the same omission the 2026-09-05 sweep fixed in `sys_accept`'s `addrlen`
(comment two functions up in `sock.rs`), in the sibling function; fixed the
same way, through `crate::uaccess::read_val`, where a bad pointer is `EFAULT`
instead of a dead machine.

**Why no suite ever caught it:** the boot suite runs in ring 0 under
`BypassValidationGuard`, so its user-pointer reads are kernel reads — no SMAP
assertion is ever exercised. Only a real ring-3 caller on a SMAP-enabled CPU
can. This is the ring-3 rule from the C2 plan, earning its keep: `apk update`
is that caller, and it is now part of the per-batch ritual.

## Verification

| gate | baseline (post-flip) | now |
|---|---|---|
| QEMU/TCG `SMP=1` | 550/0 | **553/0** (+3: round-trip + EINVAL/ENOENT pins) |
| QEMU/TCG `SMP=4` | 560/0 | **563/0** |
| Firecracker/KVM `SMP=1` | 537/0 | **540/0** |
| Firecracker/KVM `SMP=4` | 547/0 | **550/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** |
| `amd64_mem_trials --smp 4` (both arms) | 8/10, 0 unexpected | **8/10, 0 unexpected** |
| `apk update` + `apk add file` | not run at baseline | **installs; `file --version` = file-5.47** |
| clippy (`akuma-amd64`) | clean | clean |

The suite's symlink round trip now drives all four calls (`symlink` 88,
`readlink` 89, `unlink` 87) through the dispatcher by x86_64 number, so the
number hop stays asserted next to the arms that depend on it.

## Background

- `docs/archive/AKUMA_AMD64_4B_FLIP.md` — the refcount authority this batch
  sits on; glue's arms resolve `current_process_shared()`'s table, which is
  only possible because it is the only table.
- `docs/archive/APK_MISSING_SYSCALLS.md` (AArch64) — `apk` as the first
  consumer of this family, both architectures.
- `proposals/NEXT_AGENT_AMD64_4B_VFS_FOLD.md` — the remaining batches:
  `openat`/`close`, then `read`/`write`/`pread`/`lseek`, then
  `getdents64`, then `fstat`/`newfstatat`/`statfs`, then `dup`/`dup3`/
  `fcntl`. Each carries the same two questions batch 1 hit: which glue
  facade needs marking ready, and which raw user access the local arm was
  quietly tolerating.
