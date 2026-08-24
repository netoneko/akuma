# Mount: Audit, Missing Pieces, and Multiple-Disk Support

**Date:** 2026-08-24
**Status:** Audit + policy record. Nothing in this doc is implemented by this
session; §7 is the ordered build list for when mount work starts.
**Scope:** `mount(2)`/`umount2(2)` ABI, the VFS mount table, ext2, the box
mount-namespace policy, and what it would take to support a second disk.

---

## 0. Summary

| Question | Answer |
|---|---|
| Does `mount(2)` exist? | Yes — nr 40, gated on `sc-containers` (default-on; **ENOSYS on `extreme-size`**, which builds `--no-default-features`). Handles only `proc` and `tmpfs`. |
| Can anything unmount? | No. `umount2` (nr 39) returns `EPERM` unconditionally — even for box 0. |
| Can a box mount? | **No, by decision** (§2). `caller_may_mount()` restricts every mount arm to box 0. Stays that way until the box experience gets deliberate work. |
| Does `df`/`findmnt`/`mount` (listing) work? | No. `statfs` (nr 43) is undispatched, `/proc/mounts` family absent, `fstatfs` returns hardcoded fake numbers (§3). |
| How many disks does the kernel support? | **One, end to end** (§5). One `static BLOCK_DEVICE`, one ext2 instance, one root mount at boot. |

---

## 1. What exists today

### 1.1 Syscall surface

All three mount arms live in `src/syscall/container.rs` and dispatch at
`src/syscall/mod.rs:995-999`:

| Arm | Behaviour | Ref |
|---|---|---|
| `MOUNT` (40) → `sys_mount` | Box 0 only. fstype allow-list `{"proc","tmpfs"}`. `source`, `flags`, `data` **all ignored**. Normalizes the target, mounts into the *global* table. | `container.rs:156` |
| `UMOUNT2` (39) → `sys_umount2` | Always `EPERM`. Never unmounts. | `container.rs:202` |
| `MOUNT_IN_NS` (325) → `sys_mount_in_ns` | Private ABI. Box 0 only, composes *another* box's namespace: `proc`, `tmpfs`, `overlay` (via `build_overlay`, `container.rs:220`). Target `/` is the one-shot overlay re-root of the pristine `SubdirFs` jail, refused if the box has live processes. | `container.rs:260` |

The boot path mounts twice, kernel-side only (`src/fs.rs:106-114`): ext2 at
`/` (`vfs::ext2::mount` → `KernelBlockDevice` → `crate::block`), procfs at
`/proc`. There is no userspace involvement in boot mounting and no root=
selection.

### 1.2 The mount table

`crates/akuma-vfs/src/mount.rs` — one implementation, two capacities:

- Global kernel table: `MountTable = MountSet<8>` (`mount.rs:260`), behind
  `static MOUNT_TABLE` (`src/vfs/mod.rs:27`).
- Per-box namespace: `MountNamespace = MountSet<16>` held by
  `akuma_isolation::Namespace` (`crates/akuma-isolation/src/lib.rs:15`).

Properties that matter for anything built on top:

- **Longest-prefix resolution** (`resolve`/`resolve_arc`, `mount.rs:131,158`),
  mount points compared **literally** after normalization — callers must
  canonicalize or the entry is unreachable (both syscall arms do).
- **No stacking**: duplicate path → `AlreadyExists` (`mount.rs:58`). The only
  swap is `replace_pristine_root` (`mount.rs:112`), the one-shot guarded
  re-root used by `MOUNT_IN_NS` for OCI overlay roots.
- **No per-mount state**: `MountInfo { path, fs_type }` is all a mount knows.
  No source device, no flags, no options, no mount id, no refcount. This is the
  root cause behind several §3 gaps (`/proc/mounts` rendering, `MS_RDONLY`,
  `f_flags`).
- **Hard caps**: 8 global / 16 per-namespace mounts, then `NoSpace` (Linux
  says `ENOMEM`; today's mapping folds it to `EINVAL`).

### 1.3 Resolution and namespaces

`with_fs` (`src/vfs/mod.rs:185`) resolves every path: spawn-namespace override
→ process mount namespace → **fall back to the global table**. The fallback is
load-bearing and hazardous: a box whose namespace resolves nothing gets the
*host's* whole filesystem. Today that can't happen because a box namespace
always has its jail at `/` and can never lose it (`umount2` refuses, root
replace is one-shot). Any future "empty the namespace" path must preserve that
invariant.

Boxes: `create_box_namespace` (`src/vfs/mod.rs:52`) mounts a `SubdirFs` jail at
`/` when `root_dir != "/"`; herd/box compose the rest from outside via
`MOUNT_IN_NS` before the first spawn (`userspace/box/src/run.rs:188-200`).

---

## 2. Policy: mount stays unusable inside boxes

**Decision (2026-08-24): boxes may not mount, unmount, or re-root — until we
deliberately revisit the box experience.** This is already enforced and stays:

- `caller_may_mount()` (`src/syscall/container.rs:152`): every mount arm is
  box 0 only → `EPERM` otherwise.
- `umount2` refuses everyone (host included), so a fortiori boxes.
- `MOUNT_IN_NS` requires `caller_box == 0`; a live box cannot be re-rooted.
- A container cannot build a container: assembling an OCI root needs an overlay
  mount, and no box can mount at all. Nested *boxes* (process/network grouping)
  still exist; nested OCI images do not. The security argument of record is the
  doc comment at `container.rs:137-150`: a mount table is the box's whole view
  of the filesystem, and anything a box can mount it can mount *over*.

**Agreed `/proc` view for when `/proc/mounts` is implemented** (from this
session): inside a box, the mounts listing shows `/` plus exactly the mounts
composed from outside via the OCI config — **without the source directory each
mount came from**. A box learns *which* dirs are mounted into it, never *where
on the host they live*. Concretely: render `none /proc proc rw 0 0`-style rows
with `none` (or the in-box target only) as the source column; never the
`lowerdir=/var/lib/boxes/...` host paths, which is both an escape (reveals host
paths) and meaningless inside the jail.

**Design constraint for all of it:** no heap allocation on the syscall read
path — the mount table sits behind the BKL-adjacent `MOUNT_TABLE` spinlock and
`Namespace.mount`. Whatever renders `/proc/mounts` or copies mount info must
write into fixed-size buffers / the user's buffer directly (a
`copy_mounts_into(&mut [MountRow])`-style fixed-capacity snapshot), not build
`Vec`s under the lock. Same rule for any future `statfs` path.

---

## 3. Missing for `mount` to actually work (host, box 0)

Everything a normal `mount`/`df`/`findmnt`/read-only-root workflow needs, in
dependency order. (Overlaps and cross-references [`MINIMAL_DEV_BUSYBOX_APPLETS.md`](MINIMAL_DEV_BUSYBOX_APPLETS.md)
Clusters C/F/G — that doc verified the applet-level symptoms 2026-08-12.)

| # | Gap | Blocks | Where it lands |
|---|---|---|---|
| 1 | **`statfs` (nr 43) undispatched** → `ENOSYS` | `df`, anything sizing by path | Thin wrapper in `src/syscall/fs.rs` next to `sys_fstatfs` (`fs.rs:1420`): resolve path → per-fs stats |
| 2 | **`fstatfs` returns hardcoded fiction** — `f_type=0xEF53`, `f_blocks=65536`… regardless of the fd's fs; `f_flags=0` always | `df` correctness, `ST_RDONLY` detection | `Filesystem::stats()` already exists and ext2 implements it for real (`crates/akuma-ext2/src/ext2.rs:2785`). Needs: fd → fs plumbing (the fd table stores no fs/mount backref today — that's the actual work item) |
| 3 | **`MS_*` flags all dropped** — `sys_mount` takes `_flags`; `MS_RDONLY`, `MS_REMOUNT`, `MS_MOVE`, `MS_BIND` silently ignored | read-only root (CLAUDE goal), `mount -o remount,ro` | Needs per-mount flag storage (§1.2: `MountEntry` has none) + enforcement point in ext2 write paths + `ST_RDONLY` in `f_flags` |
| 4 | **`/proc/mounts`, `/proc/self/mounts`, `/proc/self/mountinfo` absent** | `mount` (listing), `df` fallback, `findmnt`, libc `setmntent` users | procfs virtual files following the `boxes`/`net/tcp` pattern (`src/vfs/proc.rs:639-692`); box view per §2 |
| 5 | **`/proc/filesystems` absent** | `mount -t auto` probing | Static list: `ext2 proc tmpfs` (+`overlay` under `sc-containers`) |
| 6 | **`/etc/mtab` absent from the image** | older busybox `mount`/`umount` read it before `/proc/mounts` | Ship as symlink → `/proc/mounts` in the disk image (populate script), not a kernel change |
| 7 | **`umount2` always `EPERM`** | `umount`, tmpfs teardown, any mount lifecycle | Real arm: box 0 + target exists + (future) busy/refcount rules. Keep refusing to drop a box's `/` (empty-namespace → global fallback hazard, §1.3) |
| 8 | **`source` argument ignored** | `mount /dev/X /mnt -t ext2`, bind mounts | Needs the device registry of §5; bind mounts additionally need `MountSet` entries that reference another fs subtree (`SubdirFs` already exists as the mechanism) |
| 9 | **`data` argument ignored** (except overlay-in-ns) | `tmpfs size=`, `mode=` | Parse in the arm, hand to `MemoryFilesystem::new` (needs a sized constructor — today it is unbounded by anything but RAM) |
| 10 | **No `mknod`, no loop devices, no devtmpfs** | classic `mount /dev/vda1` workflows | Kernel-side device naming via the §5 registry surfaced through procfs/devfs; `mknod` (nr 133) is not dispatched at all |
| 11 | **errno mapping lossy** — `NoSpace`/`AlreadyExists`/`NotFound` all fold to `EINVAL` in the arms | scriptability (`mount` reports nonsense on table-full vs busy) | Map at the arm: `NoSpace`→`ENOMEM`, `AlreadyExists`→`EBUSY`, `NotFound`→`ENOENT` |

Deliberately **not** on the list: mount propagation (shared/private/slave),
per-process namespaces via `CLONE_NEWNS`, `pivot_root`/`chroot` (neither is
dispatched anywhere), stacking mounts. None has a consumer in the current
goals; say so loudly when asked, because "we don't have them" is a decision,
not an oversight.

---

## 4. Audit findings (things that will bite the work above)

1. **`rename` cross-mount guard compares `fs.name()` strings**
   (`src/vfs/mod.rs:406`): `old_fs.name() != new_fs.name()` → `NotSupported`.
   Sound with one ext2 in the world; **wrong the moment two ext2 instances
   exist** (two disks — both named `"ext2"`, rename would be attempted against
   the wrong device). Fix when §5 lands: compare `Arc::ptr_eq` instead.
2. **Ext2 cache accounting is crate-global.** `CACHE_CAP_BYTES`,
   `CACHE_HITS/MISSES`, `CACHE_SLOTS_USED/CAP` are statics
   (`crates/akuma-ext2/src/ext2.rs:140-153`); `Ext2WriteGuard`'s orphan-lock
   recovery tracks **one** `EXT2_WRITE_LOCK_OWNER` (`ext2.rs:339`). The large
   block cache itself is per-`Ext2Filesystem::new`, so a second instance
   allocates fine — but its stats mix into one pool and a killed holder on
   disk A can force-unlock state on disk B. Per-instance owner + per-instance
   stats are prerequisites for §5.
3. **`invalidate_file_pages` resolves through `with_fs`** — it already lands on
   the right filesystem per-namespace; nothing to fix, but any new mount type
   must keep `resolve_inode` meaningful or the shared-page cache goes stale.
4. **`fstatfs` validates `buf_ptr` for 120 bytes** but the `#[repr(C)]` struct
   it writes is 120 bytes only on LP64 — fine on aarch64, don't port the
   literal blind.
5. **The mount table lock is one spinlock under the BKL world** — reinforces
   §2's no-alloc rule: snapshot with fixed-size, caller-provided buffers.

---

## 5. Multiple disks: what it would take

Today the kernel assumes **exactly one disk**, at every layer:

| Layer | Single-disk assumption | Ref |
|---|---|---|
| QEMU harness | One `-drive` (`hd0`, `virtio-mmio-bus.1`) | `scripts/cargo_runner.sh:240-241` |
| Driver | `static BLOCK_DEVICE: Spinlock<Option<VirtioBlockDevice>>` — one global; `init()` probes the **first** virtio-blk slot and stops | `crates/akuma-virtio/src/block.rs:226,234` |
| Adapter | `KernelBlockDevice` is a unit struct hardwired to `crate::block::read_bytes/write_bytes` — no device id | `src/vfs/ext2.rs:8-18` |
| FS | One `Ext2Filesystem` at `/`, mounted unconditionally from that device | `src/fs.rs:107-108` |
| ABI | `sys_mount` ignores `source`; no way to name a second device (no `/dev` nodes, no `mknod`, no devtmpfs) | §3.8/§3.10 |

The good news: the hard part already exists. `probe::probe_with`
(`crates/akuma-virtio/src/probe.rs:126`) **already scans all virtio-mmio slots**
and keeps scanning past failures — discovery machinery is multi-device-capable;
only `block::init`'s stop-at-first wastes it. `VirtioBlockDevice` itself is a
self-contained per-instance type (`capacity_sectors`, `read_bytes`,
`write_bytes` — `block.rs:58-179`). And `MountSet` mounting a *second*
`Arc<dyn Filesystem>` at `/mnt` needs no changes at all.

Tiered plan:

**Tier A — driver + adapter (mechanical):**
1. `BLOCK_DEVICE` → fixed table `BLOCK_DEVICES: [Option<VirtioBlockDevice>; N]`
   (N = 4 is plenty; slots are the real limit — 8 virtio-mmio slots total on
   QEMU virt, shared with NIC(s), sound, rump tap).
2. `block::init` loops `probe_with` to exhaustion, registering each find;
   assign stable names by discovery order (`vda`, `vdb`, …).
3. `KernelBlockDevice(u8)` carries a device index; `read_bytes/write_bytes`
   take the index. No allocation anywhere on the path.
4. `block::read_bytes_at(dev, …)`-style free functions for the syscall layer.

**Tier B — ext2 multi-instance correctness (must precede any second mount):**
5. `EXT2_WRITE_LOCK_OWNER` → per-instance field (the orphan-recovery heuristic
   misfires cross-instance today, §4.2).
6. Cache accounting statics → per-instance (or keyed by device) so
   `[FSCACHE]`/`PSTATS` stay meaningful and the RAM/8 cap is a *budget across
   instances*, not per-instance-by-accident (two instances would double the
   committed heap — the cache never shrinks, see `src/fs.rs:59-100`).

**Tier C — mount ABI:**
7. Device registry (name → `VirtioBlockDevice` index), kernel-side; surfaced
   as `/proc/devices`-style listing or a minimal devfs later. `mount(2)` parses
   `source`="/dev/vda" against it.
8. `sys_mount` gains fstype `ext2`: build `Ext2Filesystem::new` over
   `KernelBlockDevice(idx)` and mount at target. Everything below §3.4 still
   applies for *listing* it afterwards.
9. Optional: MBR/GPT partition parse (`/dev/vda1`) — pure code, no
   infrastructure, but only after whole-disk mounts work.
10. Boot: keep disk-0-as-root hardcoded initially; a `root=` notion is a
    separate project (needs cmdline plumbing the loader doesn't do yet).

**Hazards to fix alongside, not after:** §4.1 (rename name-guard), `fstatfs`
per-fs truth (§3.2), mount table `MAX=8` vs a disk-per-mount workflow,
`/proc/mounts` source rendering (§2: boxes never see host paths).

**Verification sketch when it lands:** `scripts/create_disk.sh` a second image
(data disk, ext2), attach with a second `-drive` in the runner, boot, `mount
/dev/vdb /mnt -t ext2`, write/read/umount on the host box, and confirm
`/proc/mounts` shows it while a box sees only its composed mounts.

---

## 6. Out of scope (recorded so nobody re-litigates it)

- Mount propagation / shared-subtree semantics — no consumer.
- `CLONE_NEWNS`-style per-process namespace cloning — box namespaces are the
  only namespace notion and are composed externally by design (§2).
- `pivot_root`/`chroot` — absent; box jails are the substitute.
- Loop devices, swap, `MS.lazytime`-class option fidelity.
- sysfs (`mount -t sysfs`) — separate gap, tracked in
  [`MINIMAL_DEV_BUSYBOX_APPLETS.md`](MINIMAL_DEV_BUSYBOX_APPLETS.md).

## 7. Build order when the work starts

1. `/proc/mounts` + `/proc/filesystems` + `/etc/mtab` symlink (§3.4-6) — pure
   additive, unblocks listing/`df`-adjacent tooling, exercises the fixed-buffer
   snapshot pattern before it matters.
2. `statfs` + real `fstatfs` via fd→fs backref (§3.1-2) — needed by `df` and
   by everything that checks `ST_RDONLY` later.
3. `MS_RDONLY` end-to-end (§3.3) — flags storage + ext2 enforcement +
   `f_flags`; the read-only-root goal's minimum cut.
4. Real `umount2` for box 0 with the empty-namespace invariant preserved
   (§3.7).
5. Multi-disk Tier A+B (§5.1-6) — driver table + ext2 per-instance fixes.
6. `mount -t ext2 /dev/vdX` (§5.7-8) + the §4.1 rename-guard fix in the same
   change.

## Background

Symptom-level verification of the applet side of these gaps:
[`MINIMAL_DEV_BUSYBOX_APPLETS.md`](MINIMAL_DEV_BUSYBOX_APPLETS.md) (Clusters
C/F/G). Mount-namespace security reasoning and the box-isolation audit:
[`USER_MANAGEMENT_AND_BOXES.md`](USER_MANAGEMENT_AND_BOXES.md) §3. The
overlay/jail machinery this builds on:
`crates/akuma-isolation/src/{overlay_fs,subdir_fs}.rs` and
[`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) Issue 14-15.
