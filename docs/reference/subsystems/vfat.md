# VFAT (FAT12 / FAT16 / FAT32)

`crates/akuma-vfat` — a `akuma_vfs::Filesystem` over a byte-addressed
`BlockDevice`. Exists so Akuma can edit the EFI System Partition (GRUB's
`grub.cfg`) from userspace instead of rebooting into something else.

> **Stability: C (new).** Host-tested, `fsck.vfat -n` accepts everything it
> writes, and the amd64 kernel mounts it under QEMU (virtio-blk and NVMe, see
> "Kernel wiring"). **Not yet run on the ryzen's real ESP.** Verify behaviour
> before relying on it.

## Provenance

Clean-room: written from the Microsoft *FAT32 File System Specification* (BPB,
FAT entry encodings, the 32-byte directory entry, long-name entries and their
checksum) and checked against `mkfs.fat` / `fsck.fat` as black boxes. No other
implementation's source was read; keep it that way.

## Layout

| Module | What |
|---|---|
| `layout` | Pure: boot-sector parse/validation (`Geometry`), cluster-count -> FAT type, DOS date/time, 8.3 encode/alias, long-name slot build/parse, checksum, case-insensitive compare |
| `mkfs` | `format(dev, bytes, &FormatOptions)` — 512-byte sectors, two FATs, FSInfo + backup boot sector on FAT32 |
| `fs` | `Vfat<D: BlockDevice>`: mount + the `Filesystem` impl |

`forbid(unsafe_code)`, zero budget.

## Invariants worth knowing

- **The FAT type is decided by the data-cluster count** (`< 4085` FAT12,
  `< 65525` FAT16, else FAT32), never by the `FilSysType` label.
- **One lock** (`Vfat::state`), held with `PreemptGuard`, covers everything.
  `append` is atomic because the size read and the write share one hold.
- **No cache.** The FAT is read through the device on demand (4 KiB windows for
  scans); a directory is loaded whole when looked at. The kernel's block layer
  is expected to cache sectors. Fine for a boot partition; not for a 100k-file
  volume.
- **Directory identity is a "dref"**: `0` = the fixed FAT12/16 root, otherwise
  a first cluster (FAT32's root cluster included). A `..` entry whose cluster is
  `0` means the root on every type.
- **Names.** Lookup is case-insensitive and matches the long name *or* the 8.3
  alias. A name that is all-one-case per part (`grub.cfg`) is stored as a bare
  short entry with the NT case bits; anything else gets long-name slots and a
  `~N` alias, numbered past collisions. A long-name run whose checksum does not
  match its short entry is ignored (the entry shows under its short name).
- **Free slots are `0xE5` or past the first `0x00`.** Deleting writes `0xE5`,
  never `0x00`, so entries after a deleted one stay visible.
- **Gaps read as zero.** `write_at` past EOF and `truncate` growth zero-fill
  explicitly; a shrink followed by a grow does not resurface old bytes.
- **FSInfo** (FAT32): the first mutation after mount (or after a `sync`)
  invalidates the free-count/next-free hints to `0xFFFFFFFF`; `sync` writes true
  values back. A crash between the two leaves "unknown", which is safe.
- **Inode numbers are synthetic**: `(parent dref + 1) << 32 | slot`. They are
  stable while the entry is not renamed, which is all `stat` identity needs.
- **Failed writes give back what they allocated** (`write_file_data` shrinks to
  the old size), so `NoSpace` does not leak clusters.

## Kernel wiring (amd64 only, feature `vfat`)

```bash
# ryzen: the default image plus the feature
cargo build -p akuma --target x86_64-unknown-none --release --features vfat
# anywhere else it is just added to the lean image
cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless,vfat
```

Then, as root (box 0):

```
mkdir -p /boot/efi && mount -t vfat /dev/nvme0n1p1 /boot/efi
vi /boot/efi/EFI/.../grub.cfg
umount /boot/efi        # or just `reboot`: both leave FSInfo truthful and flush NVMe
```

- **Not in `akuma-syscalls-glue`.** `mount(2)` is shared with the AArch64
  kernel, which does not get this. The amd64 dispatcher offers every `mount` to
  `crates/akuma-kernel-amd64/src/vfat.rs::try_mount` first; it answers only for
  `vfat`/`msdos`/`fat*` (and not for `MS_REMOUNT`, which glue's flag flip
  handles) and returns `None` for the rest. Without the feature the arm compiles
  to glue alone and `-t vfat` is `ENODEV`.
- **Sources.** `/dev/vdX` is a whole virtio-blk disk (a VMM test image).
  `/dev/nvme0n1pN` is GPT partition `N` through a **third window** in the NVMe
  driver (`nvme::open_aux_partition`, beside the root's and the hypervisor
  guest's): validated by the same GPT code, refused (`EBUSY`) when it is the
  root's partition or overlaps the root's or the guest's, one at a time. The
  controller is brought up on demand, so a box whose root is not NVMe can still
  mount an ESP. The window closes when the filesystem is dropped (unmount),
  flushing the controller's cache. A new class slot, not a driver `cfg`, in the
  machine half.
- **Unmount does the sync.** `umount2` does not call `Filesystem::sync`, so
  `Vfat`'s `Drop` (the last `Arc` going) refreshes FSInfo and flushes.
- **Clock.** `clock::now_us`, which is 0 before the first SNTP sync: a write
  before then is stamped 1980, the FAT epoch.

## Not done

- **Never run on ryzen's ESP.** Only QEMU. A real ESP may hold things this was
  not tried against (FAT32 with a 4 KiB-sector NVMe namespace: the crate
  supports 512..4096-byte sectors, the QEMU rig only exercises 512).
- `*_by_inode` methods (no stable inode), `link`, `symlink`, `chown`,
  `fallocate` — `NotSupported`, like the trait default. `chmod` maps only the
  write bits onto the READ_ONLY attribute.
- `..` as a path component is refused (`InvalidPath`); the VFS normalizes first.
- Sector sizes other than 512–4096, FAT mirroring flags, dirty-volume bits,
  `\\` / codepage (OEM) conversion of 8.3 names (non-ASCII short names are
  never generated; long names carry the Unicode).
- Volume-label directory entry is not created by `format` (the BPB label is).
- Subdirectory timestamps of `.`/`..` are not maintained after creation.

## Testing

```bash
cargo test -p akuma-vfat --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Kernel boot gates (QEMU, need dosfstools and e2fsprogs; both exit 0 only on a
guest `VFATPROBE PASS` **and** a clean host `fsck.vfat -n`):

```bash
FAT=16 SIZE_MIB=64 sh amd64/run-vfat.sh   # virtio-blk, -M microvm (FAT=12 SIZE_MIB=8, FAT=32 SIZE_MIB=128 also pass)
sh amd64/run-vfat-nvme.sh                 # NVMe ESP, -M q35, root on USB; also checks a decoy partition is byte-identical
```

`userspace/vfatprobe` is the guest half (mount, `mkdir`, create, rewrite
longer/shorter, append, long names, rename, unlink, `rmdir`, unmount, remount,
re-verify). It needs no busybox; on an image with one, `mount -t vfat` is the
same syscall.

`src/tests.rs` — layout vectors and a RAM-disk suite run on FAT12/16/32.
`tests/interop.rs` — formats with our `format` and runs `fsck.vfat -n` over a
workout (long names, 300-entry directory growth, rename across directories,
truncate, delete, a 3 MB file); also mounts a `mkfs.vfat` image and edits it.
Both skip with a message when dosfstools / mtools are absent. The `mcopy`
direction (files written by someone else's implementation, long names included)
is the least exercised: it only runs where mtools is installed.

## Background

Task: "edit GRUB config from Akuma" (2026-11-17). No incident, so no
`archive/` doc.
