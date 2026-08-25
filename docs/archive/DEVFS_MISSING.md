# /dev: No Devfs, Ad-Hoc Path Matching, and What a Real One Would Take

**Date:** 2026-08-24
**Status:** Audit + design record. Nothing in this doc is implemented by this
session; §5 is the ordered build list for when the work starts.

> **IMPLEMENTED 2026-08-25.** All four §5 steps landed; the body below is left
> verbatim as the design record. For current state read
> [`../reference/subsystems/vfs.md`](../reference/subsystems/vfs.md) "/dev".
> The table is `crates/akuma-vfs/src/dev.rs` (pure data, host-tested), wired at
> `src/vfs/mod.rs`'s `dev_node` / `dev_node_named` / `list_dir`.
>
> Three deviations from the plan below, all deliberate:
>
> 1. **Boxes get no synthetic `/dev` at all** (`DevProbe::in_box`) — a scoping
>    decision to keep this simple, not part of the original design. `null` and
>    `zero` still `stat` in a box so nothing regresses; `/dev/net/tap0` is
>    unaffected, so a `stack = rump` box keeps its NIC. See vfs.md.
> 2. **`open()` on `vda`..`vdd` returns `ENODEV`** rather than falling through.
>    §4 rules out a raw block fd, but once `crate::fs::exists` knows the node,
>    the generic path would hand out a `File` fd whose first `read()` fails
>    against ext2 — an error at the wrong syscall. `ENODEV` matches the
>    `/dev/net/tap0` precedent in the same function.
> 3. **`getdents64` and `sys_fchmodat` were converted too**, beyond §5's four
>    steps: the former so device nodes report `DT_CHR`/`DT_BLK` instead of
>    `DT_REG`, the latter because its `/dev/null || /dev/zero` no-op was a fifth
>    copy of exactly the drift this doc describes.
>
> Verified on QEMU (`cargo run --release`, `DISK2` attached): `ls -l /dev` shows
> `null zero random urandom vda vdb` with correct types and `1:3`/`254:16`-style
> majors; `stat /dev/random` — the headline bug — works; `cat /dev/vda` is
> `No such device`; and in a box `ls /dev` is empty with every host device
> `ENOENT` while `/dev/null` and `/dev/zero` still stat and open.
**Scope:** how `/dev` is faked today (`sys_openat`'s hardcoded path list),
what breaks because there is no real directory or stat backing it, and a
devfs design sized to fix it — modeled on `ProcFilesystem`, which already
solves the identical problem for `/proc`.

---

## 0. Summary

| Question | Answer |
|---|---|
| Does `/dev` exist as a real directory? | Only whatever the ext2 image happens to contain from the apk base layer — no device nodes are created there at boot or by any script in this repo. |
| Does `ls /dev` show anything? | **No.** `getdents64` reads the on-disk directory as-is; there is no synthetic listing layered on top the way `/proc` gets one. |
| Does `stat`/`lstat` work on a device path? | Only `/dev/null` and `/dev/zero`, and only because `sys_newfstatat` and `sys_statx` each hardcode those two paths **independently** — `/dev/random`, `/dev/urandom`, `/dev/dsp`, `/dev/audio` all `stat()` as `ENOENT` despite `open()`ing successfully. |
| Does `open()` work? | Yes, for the paths `sys_openat` special-cases by exact string match: `/dev/null`, `/dev/zero`, `/dev/urandom`, `/dev/random`, `/dev/dsp`/`/dev/audio` (if a sound device was found), `/dev/net/tap0` (if `rump` and a second NIC was found). This is unaffected by anything below. |
| Does `mount(2)`'s new `"ext2"` fstype need `/dev/vdX` to exist as a path? | **No** — `crate::block::device_index_by_name` (`crates/akuma-virtio/src/block.rs`) strips an optional `/dev/` prefix and matches the bare string `vda`/`vdb`/`vdc`/`vdd`; it never touches the filesystem. Mounting a second disk already works with no devfs. What doesn't work is a human (or a script, or `mount(8)`'s own source-validation) doing `ls /dev` or `stat /dev/vdb` first and finding nothing. |

This surfaced 2026-08-24 as a side note while reviewing the multi-disk
`mount(2)` work (`MOUNT_MISSING_SYSCALLS.md`): a device that can now be
*mounted by name* still has no path a user can `ls`, `stat`, or tab-complete.

---

## 1. What exists today

### 1.1 The `open()` path (works, stays as-is)

`sys_openat` (`src/syscall/fs.rs:1090`, per
`docs/reference/subsystems/syscalls/fs.md` "`openat` flag semantics")
special-cases these exact path strings **before** ever touching the real
filesystem, each allocating a distinct `FileDescriptor` variant with its own
read/write behavior:

| Path | `FileDescriptor` variant | Gate |
|---|---|---|
| `/dev/null` | `DevNull` | none |
| `/dev/zero` | `DevZero` | none |
| `/dev/urandom`, `/dev/random` | `DevUrandom` (same variant for both — `archive/DEV_RANDOM.md`) | none |
| `/dev/dsp`, `/dev/audio` | (virtio-sound PCM) | `crate::audio::is_available()` |
| `/dev/net/tap0` | `TapDevice` | `#[cfg(feature = "rump")]` + `akuma_net::rump_tap::is_ready()` |

This list is the closest thing to a "device table" that exists, and it's not
a table — it's five independent `if path == "..."` blocks scattered through
one large function. Nothing else in the kernel reads from it.

### 1.2 The `stat()` path (broken, and doubly duplicated)

`sys_newfstatat` (`fs.rs:2088`) and `sys_statx` (`fs.rs:2325`) **each**
hardcode `/dev/null` and `/dev/zero` inline, independently, with slightly
different literal values that happen to agree by luck:

```rust
// sys_newfstatat
if resolved_path == "/dev/null" {
    stat = Stat { st_ino: 1, st_mode: 0o20666, st_rdev: makedev(1, 3), .. };
}
```
```rust
// sys_statx, same two devices, same numbers, second copy
} else if resolved_path == "/dev/null" {
    (0o20666u16, 1u64, 0u64, 1u32, 0i64, 0i64, 0i64, 1u32, 3u32)
```

Neither function knows about `/dev/random`, `/dev/urandom`, `/dev/dsp`, or
`/dev/audio` — those fall through to `crate::vfs::metadata(path)`, which has
no `/dev` awareness at all and returns `FsError::NotFound`, so both syscalls
report `ENOENT` for a path that `open()` just accepted. `sys_faccessat2`
(`fs.rs:2443`) has the same gap one level up: it calls `crate::fs::exists`,
which resolves through the real filesystem only.

### 1.3 The `list_dir()` path (nonexistent)

`crate::vfs::list_dir` (`src/vfs/mod.rs:283`) does exactly two things: read
the real filesystem directory, then merge in mount-point children
(`get_child_mount_points`). There is no third merge step for synthetic
entries the way `/proc`'s listing effectively is one (procfs is a whole
mounted `Filesystem` impl, not a merge — see §2). `/dev` is not itself a
mount point, so nothing about the render_mounts/procfs machinery touches it
at all; `ls /dev` shows whatever is really on the ext2 image, which is
nothing this repo puts there.

---

## 2. Why this is the same problem procfs already solved

`ProcFilesystem` (`src/vfs/proc.rs`) is a real `Filesystem` impl mounted at
`/proc` at boot (`src/fs.rs`). Every entry under `/proc` — `mounts`,
`filesystems`, `<pid>/stat`, `boxes`, `net/dev`, … — goes through the same
four trait methods (`read_file`, `metadata`, `list_dir`/`read_dir`,
`exists`), so `ls /proc`, `stat /proc/mounts`, and `cat /proc/mounts` are
three views of **one** table instead of three independent hardcoded lists
that can (and did, for `/dev`) drift out of sync. `vfs.md` "procfs" states
the rule this repo already learned the hard way: *"Adding a virtual file
means touching four functions, not one."* `/dev` never got this treatment —
it grew by `sys_openat` accreting `if` blocks, syscall by syscall, over
several sessions, and nobody came back to give it the `/proc` treatment.

`/dev` doesn't need all four — nothing reads a device's *content* through the
VFS (`open()` on a device path is intercepted before the VFS is reached and
serves bytes from the `FileDescriptor` variant directly, not from
`Filesystem::read_at`), so a devfs only has to answer **"what exists"**
(`list_dir`, `metadata`, `exists`) and can leave content-serving exactly
where it already works.

---

## 3. What a fix looks like (sized, not built)

**One device table**, replacing the five scattered `if` blocks' worth of
name/major/minor/mode knowledge with a single array — something like:

```rust
struct DevNode {
    name: &'static str,      // "null", "vda", ...
    is_block: bool,           // S_IFBLK vs S_IFCHR
    perm: u32,                 // 0o666 / 0o660
    major: u32,
    minor: u32,
    ino: u64,                  // stable synthetic inode
}
```

Static entries (`null`, `zero`, `random`, `urandom` — always present) plus
dynamic ones computed at call time: `dsp`/`audio` gated on
`crate::audio::is_available()`, and — the piece that motivated this doc —
`vda`..`vdd` gated on `crate::block::device_name(idx)` for whichever indices
`crate::block::init` actually populated (`MAX_BLOCK_DEVICES = 4`,
`crates/akuma-virtio/src/block.rs`). Minor spacing `idx * 16` mirrors Linux's
convention of 16 minors reserved per disk for partitions this kernel doesn't
have. `/dev/net/tap0` is a nested path (`/dev/net/`, not `/dev/`) and stays
out of a first cut — it's already directly-openable and nothing asked for
`ls /dev/net` to work.

**Wiring**, mirroring the `/etc/mtab` interception already in
`src/vfs/mod.rs` (`is_mtab`/`mtab_rows`, not a mounted filesystem — a
resolve-time check ahead of `with_fs`) rather than standing up a whole
mounted `Filesystem` for four-ish entries:

- `list_dir`: when the resolved path is exactly `/dev`, merge the table's
  entries into whatever the real (probably empty) on-disk listing returns —
  same shape as the existing mount-point-children merge two lines below it.
- `metadata` / `exists`: a lookup keyed on the resolved path's `/dev/`-
  relative name, returned before `with_fs` ever runs — same shape as
  `mtab_rows`.
- **Syscall-layer consolidation, not just addition:** `sys_newfstatat` and
  `sys_statx`'s duplicated `/dev/null`/`/dev/zero` special cases get deleted,
  replaced by one call each into the new table-backed lookup (exposed as
  something like `crate::vfs::dev_node(path) -> Option<DevNode>`) — which
  then covers `random`/`urandom`/`dsp`/`audio`/`vda`..`vdd` for free instead
  of needing four more copy-pasted arms per syscall.

`sys_openat`'s dispatch is deliberately left alone: each device's `open()`
behavior is genuinely different (a socket-backed FD, a PRNG read loop, a PCM
sink), so collapsing it into the same table would need the table to carry
behavior, not just identity — a bigger and separately-scoped change. The
table only needs to answer "does this path exist and what does `stat` say
about it," which is the half that's actually missing.

---

## 4. Out of scope (recorded so nobody re-litigates it)

- Making `open()` on `/dev/vdX` return a working raw-block-device fd. Nothing
  in this kernel reads/writes a disk except through `Ext2Filesystem`/
  `KernelBlockDevice`; a raw block fd has no consumer today (`mount(2)`
  resolves `source` by name, not by fd — §0). Stat-and-list only.
- `/dev/net/tap0` appearing in a listing (§3, "stays out of a first cut").
  Directly-openable-only remains fine for the one caller that needs it.
- `mknod(2)` / dynamic device creation. The table is fixed at boot from
  what's actually probed (block device count, sound device presence), not a
  writable namespace.
- `udev`-style hotplug. No consumer; this kernel's device set is fixed at
  boot.
- A real mounted `DevFilesystem` (the `/proc`-shaped, heavier version of §3).
  Worth reconsidering if `/dev` ever needs `read_file`/`read_at` through the
  VFS proper (it doesn't today — see §2), or if the five `sys_openat` blocks
  ever get consolidated too and want one object to hang all four concerns
  off of.

## 5. Build order when the work starts

1. The device table itself (§3) — pure data, no wiring, easy to unit-test
   host-side (`crate::block`/`crate::audio` calls stubbed or behind a trait
   the same way other kernel-only lookups get host-tested elsewhere in this
   codebase).
2. `list_dir("/dev")` merge — the visible, demoable half (`ls /dev` stops
   being empty).
3. `metadata`/`exists` wiring — `stat`/`faccessat2` start agreeing with `ls`.
4. Delete `sys_newfstatat`'s and `sys_statx`'s duplicated `/dev/null`/
   `/dev/zero` blocks in the same change that adds their generic replacement,
   not as a follow-up — leaving both versions alive at once is exactly the
   drift this doc exists to describe.

## Background

- `docs/archive/MOUNT_MISSING_SYSCALLS.md` — the multi-disk `mount(2)` work
  this gap was noticed alongside; §5 there is where `vda`..`vdd` device
  registration comes from.
- `docs/reference/subsystems/vfs.md` "procfs" — "Adding a virtual file means
  touching four functions, not one," the rule this doc is asking `/dev` to
  finally follow.
- `docs/archive/DEV_RANDOM.md`, `docs/archive/DEV_ZERO.md` — how
  `/dev/urandom`/`/dev/random` and `/dev/zero` got their `open()` support;
  neither doc covers `stat`/`ls`, which is the gap here.
- `docs/reference/subsystems/syscalls/fs.md` "`openat` flag semantics" — the
  current, correct description of the `open()`-only special-casing this doc
  leaves untouched.
