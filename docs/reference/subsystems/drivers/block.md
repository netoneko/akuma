# Block device (VirtIO)

Source: `src/block.rs` (VirtIO block driver), `src/virtio_hal.rs` (HAL for the
`virtio-drivers` crate). For what's stacked on top (ext2, mount table,
procfs), see [`../vfs.md`](../vfs.md) "ext2" — this doc stops at the raw
sector I/O layer and does not re-explain the filesystem above it.

> **Stability: A (stable).** Dormant since 2026-03-05; only clippy-lint
> rewrites since (`.is_multiple_of`, `.div_ceil`, `Self::` — no logic change).
> No open issues. The recurring lesson: the `virtio-drivers` crate's
> `MmioTransport` negotiates v1 (legacy) vs v2 (modern) internally, so this
> driver needed **zero changes** when the QEMU runner switched
> `force-legacy=false` — only the hand-rolled RNG driver (which implements its
> own legacy-only register sequence) had to be rewritten. See
> `archive/VIRTIO_MMIO_LEGACY_TO_MODERN.md`.

## Device discovery

`block::init()` (`block.rs:234-285`) linearly probes 8 fixed VirtIO MMIO
slots (`VIRTIO_MMIO_ADDRS`, `block.rs:23-32`; each slot is `DEV_VIRTIO_VA +
n*0x200`) for `device_id == 2` (block) at offset `0x008`, takes the **first**
match, and ignores the rest — Akuma mounts exactly one block device. Once a
matching slot is found: `MmioTransport::new()` wraps the raw MMIO header, then
`VirtIOBlk::<VirtioHal, MmioTransport>::new(transport)` drives the full
virtqueue handshake (feature negotiation, queue setup, `DRIVER_OK`) via the
crate. Called from `main.rs:944`, before `fs::init()` mounts ext2
(`main.rs:940-949`) — the mount cannot succeed without a working block device.

## Wrapper type and locking

`VirtioBlockDevice` (`block.rs:76-220`) wraps `VirtIOBlk<VirtioHal,
MmioTransport>` in an `UnsafeCell` and asserts `Sync` by hand
(`block.rs:81-84`) — `VirtIOBlk`'s read/write API takes `&mut self`, but the
driver is shared through one global `Spinlock<Option<VirtioBlockDevice>>`
(`BLOCK_DEVICE`, `block.rs:226`). The `Spinlock` is the actual synchronization;
`inner_mut()`'s unsafe cast just gets past the borrow checker for callers that
already hold the lock (`with_device`, `block.rs:294-300`). There is no
separate request queue or async completion path — `read_sectors`/
`write_sectors` block the calling thread until the virtqueue round-trip
completes.

## Sector vs. byte API

| Fn | Alignment | Notes |
|---|---|---|
| `read_sectors` / `write_sectors` (`block.rs:121-172`) | caller's buffer must be a `SECTOR_SIZE` (512 B) multiple | direct passthrough to `VirtIOBlk::read_blocks`/`write_blocks`; bounds-checked against `capacity_sectors` |
| `read_bytes` / `write_bytes` (`block.rs:175-219`) | arbitrary offset/length | rounds out to a sector-aligned range, does a full sector read into a temp `Vec`, splices in the requested slice, and — for writes — writes the whole temp buffer back |

`write_bytes` is a **read-modify-write**: any partial sector at either end of
the range is read first so the surrounding bytes aren't clobbered. This is
the only API `KernelBlockDevice` (`src/vfs/ext2.rs:7-18`) uses — ext2 (and
anything else built on `akuma_ext2::BlockDevice`) always goes through the
byte-offset path, never the raw sector one. See [`../vfs.md`](../vfs.md) for
how ext2 turns these byte reads/writes into a filesystem.

## HAL: DMA and MMIO address translation

`virtio_hal.rs` implements the four methods `virtio-drivers::Hal` requires,
all in terms of `akuma_exec::mmu::{phys_to_virt, virt_to_phys}`:

- `dma_alloc`/`dma_dealloc` — page-aligned `alloc_zeroed`/`dealloc` from the
  kernel heap, converting the returned virtual address to physical via
  `virt_to_phys` for the device's descriptor table (`virtio_hal.rs:16-46`).
- `mmio_phys_to_virt` — `phys_to_virt(paddr)` (`virtio_hal.rs:48-51`); trivial
  because the kernel identity-maps RAM.
- `share`/`unshare` — `share` just translates a buffer's VA to PA for a
  descriptor; `unshare` is a no-op (no cache-coherency management needed on
  QEMU, `virtio_hal.rs:61-67`).

This entire HAL is built on the assumption that `phys_to_virt`/`virt_to_phys`
are identity (`VA == PA` for kernel-heap addresses) — see
`archive/IDENTITY_MAPPING_DEPENDENCIES.md` "VirtIO HAL DMA Allocation". If the
kernel ever moves off identity mapping (that doc's proposed TTBR1 route), this
file is one of the fixed call sites that would need no change *by contract*,
but should be re-verified: DMA buffers must be addresses the device can be
told the physical address of, and MMIO register access must resolve to the
actual device frame, not a stale identity assumption.

## Capacity and errors

`capacity_sectors()`/`capacity_bytes()` are cached at construction
(`block.rs:88-94`) from `VirtIOBlk::capacity()` — not re-queried per I/O.
`BlockError` (`block.rs:42-66`) distinguishes `NotFound` (init couldn't find a
slot 2 device), `NotInitialized` (`BLOCK_DEVICE` is `None` — called before
`init()` or init failed), `InvalidOffset` (misaligned buffer or out-of-range
sector), and `ReadError`/`WriteError` (the crate's `read_blocks`/
`write_blocks` returned `Err`); all sites log via `safe_print!` before
returning the error, since these are the deepest layer above raw MMIO and
have no context to add once wrapped by the VFS.

## Background

- `archive/VIRTIO_MMIO_LEGACY_TO_MODERN.md` — why block/net needed no changes
  when the RNG driver had to learn the modern (v2) transport.
- `archive/IDENTITY_MAPPING_DEPENDENCIES.md` — the identity-mapping
  assumption this HAL is built on.
- `archive/EXT2_FIRST_DATA_BLOCK_FIX.md` — an ext2-layer bug (off-by-one block
  numbering) that looked like a block-device I/O error; see
  [`../vfs.md`](../vfs.md) instead, this driver was not at fault.
