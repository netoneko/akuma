# Credentials and file permissions

**Stability: B** — verify behaviour. Landed 2026-10-14; checked on AArch64
(QEMU/HVF, Firecracker/KVM) and amd64 (QEMU, the trashcan metal) with
`permprobe`, whose 50 checks match Linux line for line.

Unix credentials (uid, gid, supplementary groups, umask) are real per-process
state; file ownership is recorded and reported; and owner/group/other mode
bits are enforced **when the switch is on** — it starts off.

## The model in five rules

1. **The box is the boundary; uid lives inside it.** Every permission check
   here is in addition to the box checks (`containers.md` → "Box
   permissions"), never instead of them. Uid numerals are box-scoped with no
   remapping: uid 999 in two boxes are different principals that never meet,
   because box roots are disjoint.
2. **Real = effective = saved.** One value per id. A call that would split
   them (`seteuid(1000)` keeping a saved 0, `setresuid(0, 1000, -1)`) is
   `EPERM`. There is no saved uid to climb back to.
3. **Only uid 0 changes anything, once.** `setgroups` → `setgid` → `setuid`
   from root drops; after that, every setter accepts only the value already
   held (musl's `__synccall` repeats the call on every thread — those repeats
   must succeed).
4. **uid 0 bypasses every check but one**: exec of a file with no x bit at all.
   That is Linux's rule, and it is why turning enforcement on changes nothing
   for a root-only workload.
5. **No set-user-ID on exec, ever.** `S_ISUID`/`S_ISGID` are stored and
   reported, never honoured. `execve` and box entry leave credentials alone.

## Where things live

| Piece | Location |
|---|---|
| `Creds`, transition rules, `SharedCreds` | `crates/akuma-isolation/src/creds.rs` (host-tested) |
| The checks: `may`, `may_delete`/`sticky_allows`, `may_chmod`/`chmod_mode`, `may_chown`, `may_set_times`, `may_signal`, the switch | `crates/akuma-isolation/src/dac.rs` (host-tested) |
| `Process::creds: Arc<SharedCreds>` — fork copies, `clone_thread` shares, spawn copies the caller's | `crates/akuma-exec/src/process/mod.rs`, `spawn.rs`; amd64 `register_exec_process` |
| Credential syscalls | `crates/akuma-syscalls-glue/src/cred.rs` |
| Which check each path syscall makes | `crates/akuma-syscalls-glue/src/perm.rs` + call sites in `fs.rs`, `unixsock.rs`, `proc.rs` |
| `kill`/`tkill`/`tgkill`/`pidfd_send_signal` | `akuma_exec::process::check_signal` (both kernels) |
| Ownership storage | `Metadata.{uid,gid}`, `Filesystem::chown`; ext2 (incl. `osd2` high halves), memfs, overlay, `SubdirFs` |
| `/proc/<pid>/status` `Uid`/`Gid`/`Groups`/`Cap*` | `crates/akuma-procfs` |
| The switch | `/proc/dac` (host root only): `echo 1 > /proc/dac` |

## What is checked

Only while `/proc/dac` reads `1`, and only for a non-root caller (root needs a
metadata read for exec alone):

| Call | Check | Error |
|---|---|---|
| `open` existing | read/write by access mode; `O_TRUNC` ⇒ write | `EACCES` |
| `open(O_CREAT)`, `mkdir`, `symlink`, AF_UNIX `bind` | write+search on the parent | `EACCES` |
| `unlink`, `rmdir`, `rename` | write+search on the parent; then the sticky rule | `EACCES`; sticky `EPERM` |
| `chmod` | owner or root (non-member loses `S_ISGID`) | `EPERM` |
| `chown` | root; owner may change group to one it is in | `EPERM` |
| `truncate` | write | `EACCES` |
| `utimensat` | owner, or write access when setting "now" | `EPERM` / `EACCES` |
| `access`/`faccessat` | the same check, by `R_OK`/`W_OK`/`X_OK` | `EACCES` |
| `chdir` | search | `EACCES` |
| `execve` (both kernels' paths) | execute | `EACCES` |
| `kill` family | **box reach (always on)**, then same uid or root | `ESRCH` / `EPERM` |

Always on regardless of the switch: credentials themselves, the box half of
`kill`, ownership recording (`stat`, `chown`), new files owned by their creator,
`umask` applied to `open(O_CREAT)` and `mkdir`.

### Known divergences (pinned, under-deny only)

- **Search permission on intermediate directories is not checked** — only the
  final object and its parent. A readable file inside an unsearchable
  directory opens.
- A **root** creator in a set-group-ID directory keeps gid 0 rather than
  inheriting the directory's group.
- `setfsuid`/`setfsgid` report and change nothing (fs ids are the effective ids).
- `capset` stays an accepted no-op; `capget` is full at uid 0, empty otherwise.

## Containers

`box run` starts the image's `config.User` (or `-u user[:group]`), resolved
against the **image's** `/etc/passwd` and `/etc/group` (`boxlib::spec::resolve_user`,
runc's rules). An unknown name is an error, never root. The credentials ride in
`SpawnOptions` (112 bytes; `uid`/`gid`/`groups_ptr`/`ngroups`/`cred_flags` at
88..112) and are installed before the child is published. A spawn may only
lower credentials — the `setuid` rules apply.

Layer extraction keeps numeric owners (`akuma-tar`, as root) and directory
modes, so an image's `/tmp` is `1777` and `/etc/shadow` is `0640 0:42`.
**Images pulled before 2026-10-14 are root-owned with default directory
modes; re-pull them.**

## Before turning enforcement on by default

Disks written while modes were ignored hold binaries without x bits: the
trashcan had 90 files at `0644` in `/bin`, including `/bin/rm`, which root
cannot exec once the switch is on. Fix modes on existing disks (`chmod +x` the
ELF files under `/bin`, `/sbin`, `/usr/bin`) first, then flip the default in
`akuma_isolation::dac`.

## Verify

```sh
userspace/build.sh            # stages /bin/permprobe
ssh … root@guest /bin/permprobe   # expect "permprobe: 0 fail"; restores /proc/dac
```

Linux reference run: `docker run --rm --platform linux/arm64 -v
$PWD/userspace/forktest/c_stress:/p alpine /p/permprobe` — the same 50 lines.

## Background

- [`../../archive/UNIX_PERMISSIONS.md`](../../archive/UNIX_PERMISSIONS.md) — design and the implementation record
- [`../../../proposals/USER_MANAGEMENT_AND_BOXES.md`](../../../proposals/USER_MANAGEMENT_AND_BOXES.md) — the identity model this builds on
- [`containers.md`](containers.md) — box permissions, which come first
