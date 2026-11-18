# The platform kernels stopped building, and nothing noticed (2026-11-18)

## Symptom

`denver` failed to build from `main` (`aa95540f`) on **both** architectures, with the default feature set and with the
JVM overlay's (`overlays/denver/build-kernel.sh`). It was found by building from a clean worktree; a plain `cargo build`
at the root never reaches it, because `taoxichuan`, `shanghai` and `denver` are workspace members but not
`default-members`. A fifth failure, in the rump devbox build, was found by the script written to stop this recurring.

## Cause

Six independent drifts, each an API migration in a shared crate that did not update every caller. The platform kernels
share `akuma-syscalls-glue`, `akuma-exec` and `akuma-tokio-kernel-core` with the main kernel, so a change there is only
checked against the callers someone happened to build.

1. **`akuma-syscalls-glue/src/proc.rs`**: `use ...::box_access::{BoxId, CallerBox, SpawnBox}` is used only by the box
   syscalls, which are `akuma-native`'s. A platform kernel builds glue without that feature, and the workspace sets
   `unused_imports = "deny"`: a hard error on every platform kernel (it was the first failure, and hid the rest).
2. **`akuma-tokio-kernel-core` file-page cache**: `akuma_fpcache::insert`, `akuma_fpcache_rw::Key::new` and
   `Table::for_each_in` now take a `FileKey` (the typed `(mount, inode)`); `lazy.rs` and `shmpages.rs` still passed raw
   `u32` pairs, and `shmpages::wrote`/`zeroed` had been half-migrated (a `FileKey` parameter, a body using the old
   calls).
3. **`akuma-tokio-kernel-core` process calls**: `clone_thread`, `fork_process` and `execve::read_request` take
   `UserAddr` (and `read_request` answers `Errno`, not `u64`); `proc.rs` passed `u64`.
4. **`Process::auxv`** (`procfs-diag`) was assigned through the shared `&Process` that `with_process` hands out; it is
   a `Spinlock<Option<..>>` now (clone, init and both readers/writers updated).
5. **The two tokio kernels' arch halves**: `read_at_by_inode`/`read_at_by_mount_inode` take a `FileKey`
   (aarch64 `exec.rs`), and `sys_getcpu` takes `UserAddr` (aarch64 `syscall.rs`, amd64 `lib.rs`).
6. **`akuma-syscalls-glue/src/net.rs`, the no-smoltcp `sendto`/`recvfrom`** (the rump devbox build) unwrapped the
   `UserAddr` with `.get()` and passed the `u64` on to `unix_send_user`/`unix_recv_user`, which take the typed address.

## Fix

Each caller moved to the new API (a zero mount or inode, which `FileKey` cannot express, keeps the old behaviour: no
share, or the hook's `-1`); item 1 is a `#[cfg(feature = "akuma-native")]` on the import, matching its use.

`scripts/premerge_build.sh` builds every kernel and feature set (16 steps: the main kernel on both architectures, the
devbox and ryzen sets, `taoxichuan`, `shanghai` with its middleware sets, `denver` default and JVM sets, each on both
architectures) and is a step before merging to main (`CLAUDE.md`). Its first run found item 6.

## Verified

`scripts/premerge_build.sh`: 16 of 16 build, from this branch's tree, on 2026-11-18.

## Still open

Built, not booted: no Denver, Taoxichuan or Shanghai kernel was run after these changes, and the JVM gate was not
re-run. The changes are type-level (a typed address or key where a raw integer was), plus the `auxv` lock; a boot of
each is the check the build cannot give. Host tests of the touched crates were not re-run beyond the ones listed in
the branch's other docs.
