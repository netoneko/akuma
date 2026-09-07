# amd64 C1 step 4a: the private mount table is gone

**Date:** 2026-09-07
**Scope:** `amd64/src/fs.rs`'s own `MountTable` and its twelve `with_fs`
wrappers, replaced by `akuma-vfs-glue` — the crate the AArch64 kernel's VFS
already is, and the one `akuma-syscalls-glue::fs` resolves through.
**Status:** landed and verified on four rigs.
**Background:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` § C1 step 4;
`proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md` § "Suggested order" item 4.

---

## 1. Why this is step 4's prerequisite and not a tidy-up

The hand-off prompt's step 4 is "fold the VFS surface into glue: `fs.rs` is
3,112 lines against `fd.rs`'s hand-rolled equivalents". Reading glue's `fs.rs`
for what a folded arm actually touches gives two dependencies, not one:

| glue `fs.rs` names | count | amd64 had |
|---|---|---|
| `akuma_exec::process::current_process_shared` | 41 | nothing — no `akuma-exec` process is registered on this target |
| `akuma_vfs_glue::…` | 94 | a **different** mount table, private to `amd64/src/fs.rs` |

The first is the process table and is genuinely large (it is what step 5 builds).
The second is not large at all — and it is the one that fails *silently*. A
folded `sys_openat` resolves through `akuma_vfs_glue::with_fs`, which consults
`akuma-vfs-glue`'s `MOUNT_TABLE`. On a kernel whose root is mounted into a
different table, that resolves to nothing and the arm returns `ENOENT` for every
path on the disk. Not a compile error; not a panic. The same failure shape as
C1 step 1's syscall numbers.

So the two tables had to become one table before any VFS arm can be folded, and
that is this change. It is also the smaller half by a wide margin: the private
table was **195 lines of code** and all of it had a counterpart.

## 2. What was deleted

`amd64/src/fs.rs` carried:

- `static MOUNTS: Spinlock<Option<MountTable>>`
- `fn with_fs<R>(path, f)` — resolve, then call the filesystem **with the lock
  still held**
- twelve wrappers whose bodies were one `with_fs` call each: `read_file`,
  `write_file`, `create_dir`, `remove`, `rename`, `create_symlink`,
  `read_symlink`, `set_times`, `metadata`, `read_dir`, `stats_for_path`,
  `render_mounts`, plus `for_each_mount`/`mount_count`

All of it is `pub use akuma_vfs_glue::{…}` now. Three wrappers survive and each
has a reason written at it: `remove(path, rmdir)` (the crate splits what
`unlinkat` joins), `render_mounts(buf)` (the crate takes a viewer box id and a
target pid this target has no value for yet), and `mount_count()` (the crate
deliberately exposes no table length — see §6).

The non-test half of the file went **195 → 125 code lines**. What is left is the
part that is genuinely this target's: three `BlockDevice` shims (`VirtioBlk`,
`UsbDisk`, the `RootDevice` enum over them), the wall-clock source ext2 stamps
inodes from, and `mount_root_on`.

## 3. What arrived with it, none of it written here

Every one of these was absent before and is now covered by a boot check:

- **The lock is no longer held across disk I/O.** The old `with_fs` handed a
  borrowed `&dyn Filesystem` out of the guard, so every read ran under the
  mount-table spinlock — precisely the hazard CLAUDE.md records against the
  AArch64 `MOUNT_TABLE`. `resolve_mount` clones the `Arc` and drops the lock
  before the filesystem call. The cost is one `String` per resolution, which the
  old `rename` was already paying by hand.
- **A real path walk.** `..`, `.`, `//` and a trailing slash are normalised
  before resolution. The old table saw whatever string `fd.rs` handed it, so
  `/bin/../probe.txt` was a lookup of a directory entry literally named `..`.
- **Symlink following in `open(2)`** — §4, the one that turned out to be a live
  bug rather than a missing feature.
- **Synthetic `/dev`.** Resolve-time nodes rather than a mounted filesystem, so
  `ls /dev` and `stat /dev/null` answer. This is the *existence* half of
  `AKUMA_SELF_HOSTING_AMD64.md` open issue 2; the byte-serving half is §5.
- **Synthetic `/etc/mtab`**, the same rows as `/proc/mounts`, which `mount(8)`
  with no arguments reads.
- **`MS_RDONLY` is enforced.** Every write chokepoint goes through
  `with_fs_write`, which refuses a read-only mount with `FsError::ReadOnly` →
  `EROFS`. The private table *recorded* the flag and never consulted it.

## 4. `ln -s` created links nothing could read

`readlinkat` on this target called `fs::read_symlink` directly and worked.
`open` did not follow links at all: `sys_openat` handed the link's own path to
`read_file`, which on a link inode is `NotAFile`. So `ln -s` succeeded,
`readlink` printed the target, and `cat` through the link reported `ENOENT`.

Measured before the fix, over ssh on the local QEMU rig:

```
$ ln -s /tmp/w1.txt /tmp/link1; readlink /tmp/link1; cat /tmp/link1
/tmp/w1.txt
cat: can't open '/tmp/link1': No such file or directory
```

The fix is one call to `akuma_vfs_glue::resolve_symlinks` — the same function
the AArch64 `sys_openat` has always run — placed in `sys_openat` and
**deliberately not** in `resolve_at`. That helper also serves `symlinkat`,
`readlinkat` and `unlinkat`, and every one of those operates on the link itself:
following there would make `rm` delete the target. The `rm /tmp/l1` check in §7
is what pins that, and it would pass just as happily against the bug if the only
thing checked were that the link disappears — it also checks the target survives.

`O_NOFOLLOW` is honoured by skipping the walk rather than by failing with
`ELOOP` on a link. That is the weaker half of the flag and is stated in the code
as a pinned divergence rather than assumed absent; this target has no `O_PATH`
and nothing that opens a link in order to inspect it.

Only one bug, which breaks this step's three-for-three streak from C1 step 3 —
but it is the same shape: the fold reads a record (a symlink inode) that
something else in the tree already knew how to write.

## 5. Where `/dev` stops, asserted rather than assumed

`akuma_vfs::dev` is pure data by design — its module header says so — and
serving a device's *bytes* is `sys_openat`'s dispatch on both kernels. amd64 has
no such arm. So after this change `/dev/null` **lists** and **stats** and cannot
be **opened**, which is a real half-state and the kind that rots quietly.

It is a boot check:

```
dev: reading a device node's bytes is still unwired   [OK]
```

Delete that check in the change that wires `sys_openat`. If it starts failing on
its own, something began answering and every comment around it is stale.

Two smaller gaps recorded rather than closed: `ls -la /dev` prints `0, 0` for
major/minor, because `fd.rs`'s `encode_stat` fills `st_rdev` from
`akuma_vfs::Metadata`, which carries no `rdev` — the AArch64 kernel calls
`dev_node` directly for the full `stat` and this target does not yet. And a box
carve-out (`DevProbe::in_box`) that cannot fire here because there are no boxes.

## 6. Three decisions worth carrying

**`/proc` did not come with the crate.** `akuma-vfs-glue` has a
`ProcFilesystem`, it compiles for `x86_64-unknown-none`, and mounting it would
be a regression: it renders from `akuma-exec`'s process table, which this target
does not populate, so it would replace `fd.rs`'s synthetic `/proc` — which reads
*this* kernel's tables and works — with one that reports an empty machine. It
becomes right in the same step that gives this target `akuma-exec` processes.

**`mount_count` counts rendered rows, not table entries.** The crate exposes no
table length, deliberately: every consumer wants the *visible* set, which is a
function of the asking process's namespace rather than of the table. Counting
`/proc/mounts` lines asks the question the callers have and exercises the path
`df` takes. (Eight mounts is the table's ceiling, so clippy's `bytecount`
suggestion is answered with an `allow` and a reason.)

**Mount sources gained a `/dev/` prefix** — `vda` → `/dev/vda`, `sda1` →
`/dev/sda1`, matching the AArch64 kernel. Not cosmetic:
`akuma_vfs_glue::device_is_mounted` strips that prefix to decide whether a raw
block open would race a filesystem's own cache.

**`fs::init_vfs()` is called from `boot::install_shared_sinks`**, not from
either `kmain`. That function exists because C1 step 3's first folded arm was
registered on the PVH path and not the multiboot2 one, and the metal died at the
first folded syscall. Registering the VFS from one entry point would have
reproduced it exactly — and the failure would have been a `DISK=none` /
bare-metal `FsError::NotInitialized`, not a panic naming itself.
`mount_root_on` calls it again for safety; both halves are idempotent
(`OnceCopy`, and `init()` only fills an empty table).

## 7. Verification

Boot self-tests, `SMP=4` on every rig. The suite gained 20 checks
(`path:` ×10, `dev:` ×8, `/etc/mtab`, writable-mount) and lost one (`mount: /
resolves a path unchanged`, subsumed by the stronger `path:` family):

| rig | entry | before | after |
|---|---|---|---|
| local QEMU/TCG `SMP=4` | PVH | 453 / 0 | **472 / 0** |
| Firecracker on the box | PVH | — | **459 / 0** |
| OVMF/GRUB q35 `SMP=4` | multiboot2 | 444 / 0 | **463 / 0** |
| HP box, bare metal `SMP=4` | multiboot2 | 444 / 0 | **463 / 0** |

Ring 3 on the bare metal, because the suite's own checks run under
`BypassValidationGuard` and a folded path is only proven by a real caller:

```
$ ls -la /dev          # six nodes; this path was ENOENT before
$ df                   # module  524288  19748  504540  4%  /
$ cat /etc/mtab        # module / ext2 rw 0 0
$ ln -s /tmp/m.txt /tmp/ml; cat /tmp/ml   # hi
$ rm /tmp/ml; ls /tmp                     # m.txt survives — the link went, not the target
```

Host tests **143 suites / 1360 passing, 0 failures** — unchanged, as expected:
every new check is a boot self-test, because what is being tested is *this
kernel's wiring* and the crate's own logic already has host coverage.

**The AArch64 kernel cannot be affected.** The diff touches `amd64/`,
`Cargo.lock`, and one comment in `crates/akuma-vfs-glue/Cargo.toml`; no shared
crate's source changed, so no section compare was needed. `cargo build --release`
and `cargo clippy --release` are clean, and
`cargo check -p akuma-syscalls-glue --target x86_64-unknown-none` — the gate —
stays green.

## 8. What did not move, and what is next

`amd64/src/fd.rs` is untouched apart from the call-site conversions and the
symlink follow. Its 3,512 lines are the in-memory file cache and the syscall
arms, and both wait on the process table: glue's `fs.rs` reaches
`current_process_shared()` 41 times, for the fd table, the CWD and the
namespace. That is step 5's work (`Spawn`/`PROCS` → `akuma-exec`), and it is now
the *only* thing between here and folding the VFS arms.

Two things worth doing before or alongside it, both small and both now unblocked
by the crate being present:

1. **Wire `sys_openat` for the device nodes** (`akuma_vfs_glue::dev_node` →
   `FileDescriptor::DevNull`/`DevZero`/`DevUrandom`/`DevTty`), which closes the
   other half of open issue 2 and lets `> /dev/null` stop buffering bytes into a
   file that then fails to persist.
2. **Fill `st_rdev` from `dev_node`** in `fd.rs`'s `encode_stat`, so `ls -l`
   stops printing `0, 0` for every device.

## Background

- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the unlock tree; C1 is its box.
- `proposals/NEXT_AGENT_AMD64_C1_USERMODE_FOLD.md` — the hand-off this executes
  item 4 of.
- `docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md` — why
  `install_shared_sinks` exists, and the boot-protocol drift it was built for.
- `docs/archive/DEVFS_MISSING.md` — the `/dev` table this target inherited, and
  why `open()` behaviour is deliberately not in it.
