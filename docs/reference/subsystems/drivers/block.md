# Block device (VirtIO)

Source: `crates/akuma-virtio/src/block.rs` (the driver and the device table),
`crates/akuma-virtio/src/{mmio,ring}.rs` (transport and queue memory), and
`crates/akuma-virtqueue` (the split queue and the virtio-blk wire format, pure and
host-tested). For what's stacked on top (ext2, mount table, procfs), see
[`../vfs.md`](../vfs.md) "ext2"; this doc stops at the raw sector I/O layer.

> **Stability: B (verify behaviour).** Driven in-tree since 2026-11-01, replacing
> `virtio-drivers`' `VirtIOBlk` ([`AKUMA_VIRTIO_IN_TREE.md`](../../../archive/AKUMA_VIRTIO_IN_TREE.md)).
> Booted on QEMU `virt` (AArch64), QEMU `microvm` (amd64, Taoxichuan); not yet on
> Firecracker or kvm0.

## Transport: virtio-mmio version 2 only

A slot is used only if it shows the magic `"virt"`, version 2 and a non-zero device
id. **QEMU must be started with `-global virtio-mmio.force-legacy=false`**, or its
devices are version 1 and the boot log says `is not virtio-mmio v2`. Firecracker
and kvm0 are version 2 only.

The status register is written one milestone per write (`ACKNOWLEDGE`, `+DRIVER`,
`+FEATURES_OK`, `+DRIVER_OK`): Firecracker discards any other write and leaves the
device dead while config reads still work. The only feature the driver asks for is
`RO`; `VERSION_1` is required.

## Device table

`block::init()` registers **every** virtio-blk the machine exposes, in slot order,
as `vda`, `vdb`, … up to `MAX_BLOCK_DEVICES` (4); `vda` is the boot disk, the others
are mounted at runtime with `mount(2)`. Each device has a `Spinlock` in
`BLOCK_DEVICES`; that lock is held across a whole request (see the BKL-free
VFS note on the static).

## A request

One request in flight per device, three descriptors: the 16-byte header (device
reads), the caller's data buffer (device writes it for a read, reads it for a
write), and one status byte (device writes). The queue memory, the header and the
status byte are `DmaArray` statics indexed by the device's ring slot; nothing is
allocated per request. The driver spins on the used ring until **its** head comes
back, so the caller's buffer is the data descriptor directly: it cannot be freed
before the device is done with it. A status other than `S_OK` (the byte is preset
to `0xff`) is `ReadError` / `WriteError`; a write to a read-only device is refused
before it is sent.

The data buffer's device address is `virt_to_phys` of its start, which assumes a
kernel buffer is physically contiguous where it is virtually contiguous (identity
map on AArch64, the physmap on amd64).

## Sector vs. byte API

| Fn | Alignment | Notes |
|---|---|---|
| `read_sectors` / `write_sectors` | buffer a multiple of `SECTOR_SIZE` (512 B) | one request; range-checked against `capacity_sectors` without overflow |
| `read_bytes` / `write_bytes` | arbitrary offset/length | aligned calls (every ext2 call) go straight to the sector path; a misaligned one reads whole sectors into a temporary, and a write is read-modify-write |

## Capacity and errors

`capacity_sectors()` is read once from config space at registration.
`BlockError`: `NotFound` (no device came up), `NotInitialized` (no device at that
index), `InvalidOffset` (misaligned or out of range), `ReadError` / `WriteError`.
Every path logs with `safe_print!` before returning.

## Background

- [`AKUMA_VIRTIO_IN_TREE.md`](../../../archive/AKUMA_VIRTIO_IN_TREE.md) — why the
  driver is in-tree, what was verified, what is still open.
- `archive/VIRTIO_MMIO_LEGACY_TO_MODERN.md` — the move to the modern transport.
- `archive/AKUMA_FIRECRACKER_KVM.md` — the stepped status handshake.
- `archive/EXT2_FIRST_DATA_BLOCK_FIX.md` — an ext2 bug that looked like a block
  I/O error; this driver was not at fault.
