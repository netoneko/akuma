# VFAT (FAT12 / FAT16 / FAT32)

`crates/akuma-vfat` — a `akuma_vfs::Filesystem` over a byte-addressed
`BlockDevice`. Exists so Akuma can edit the EFI System Partition (GRUB's
`grub.cfg`) from userspace instead of rebooting into something else.

> **Stability: C (new, not yet wired into a kernel).** The crate is host-tested
> and `fsck.vfat -n` accepts everything it writes, but **no kernel mounts it
> yet** — see "Not done" below. Verify behaviour before relying on it.

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

## Not done

- **Kernel wiring.** Nothing registers `Vfat` with `mount(2)`'s `fstype`
  switch or provides the ESP's `BlockDevice` adapter (the GPT partition window
  on NVMe/virtio). That, plus a clock (`now: fn() -> u64`), is the next step for
  "edit grub.cfg from Akuma".
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
