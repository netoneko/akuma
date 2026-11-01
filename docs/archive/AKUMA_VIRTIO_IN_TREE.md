# virtio-blk and virtio-net driven in-tree (2026-11-01)

The disk and the NIC on every VMM target (QEMU `virt`, QEMU `microvm`, Firecracker,
Akuma's own `/dev/kvm0`) were driven by the external `virtio-drivers` 0.13 crate. They
are now driven by two in-tree crates, and `virtio-drivers` is compiled only for
virtio-sound (AArch64's `sound` feature).

## Why

Measured with `scripts/cloc_akuma.py --audit taoxichuan`, `virtio-drivers` and what it
alone pulled in were the largest third-party block left in the Taoxichuan image:

| crate | audit lines |
|---|---|
| zerocopy 0.8.56 | 8,334 |
| virtio-drivers 0.13.0 | 3,550 |
| bitflags 2 | 1,752 |
| safe-mmio 0.3.1 | 746 |
| thiserror 2 | 139 |
| **total** | **14,521** (about 900 of them inside `unsafe`, invisible to `check_unsafe_budget.py`) |

Akuma used a small part of it: the virtio-mmio transport, `VirtIOBlk` (read, write,
capacity) and `VirtIONetRaw` (begin/complete receive and transmit, a blocking send).
The tree already had two hand-written virtio paths that worked around or beside it:
`akuma-virtio/src/rng.rs` (a whole driver, because 0.7 had no RNG device) and
`akuma-net-nic/src/virtio_rings.rs` (static frame rings, because the crate's one-buffer
send spun with `NETWORK` held). See `AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md` row 10.

## What replaced it

- **`akuma-virtqueue`** (new, `forbid(unsafe_code)`, no dependencies, 17 host tests): the
  virtio-mmio v2 register map, the status handshake order, feature negotiation, the split
  virtqueue over a 16-bit-cell memory trait, and the virtio-blk / virtio-net wire formats.
  The queue shadows its free list and chain links, so it never reads a descriptor back from
  shared memory; a used element naming a head that is not in flight is refused and frees
  nothing. The tests drive it against a simulated device that walks chains through the
  shared table, including 70,000 requests across both 16-bit index wraps with out-of-order
  completion.
- **`akuma-virtio`**: `mmio.rs` (the transport over an `MmioWindow`, status written one
  milestone per write for Firecracker), `ring.rs` (each queue a `DmaArray` static, claimed
  once), `block.rs` (one request at a time, three descriptors, busy-polled) and `net.rs`
  (`NetRaw`, the `VirtIONetRaw` method set and contracts unchanged, so `Nic` and the rump
  tap changed only their type). `transport.rs`, `hal.rs` and the `zerocopy` dependency are
  behind `sound`.

## Two things the switch had to get right

1. **Modern transport only.** QEMU's virtio-mmio is the legacy (v1) transport unless it is
   started with `-global virtio-mmio.force-legacy=false`. `virtio-drivers` spoke both; the
   new driver speaks v2, as Firecracker and kvm0 do and as `rng.rs` already required. The
   AArch64 runner and `amd64/run.sh` already passed the flag; the two Taoxichuan rigs
   (`scripts/kvm/taoxichuan/tcg_rig.py`, `scripts/kvm/taoxichuan_pop.sh`, and the ryzen's
   patched copy `/root/tx-pop.sh`) did not, and the first Taoxichuan boot found no disk
   (`[virtio] slot 0: device 2 is not virtio-mmio v2`). The message now names the flag.
2. **`send` prepends the header.** `VirtIONetRaw::send` takes a bare Ethernet frame and
   chains its own zeroed `virtio_net_hdr` in front; `transmit_begin` sends a buffer
   verbatim, header included. The first `NetRaw::send` sent the frame verbatim, so QEMU read
   the frame's first 12 bytes as the header: DHCP was never answered and ssh never
   connected, while the disk on the same ring code worked. `send` now chains a static zero
   header descriptor ahead of the frame.

## Verified

- Taoxichuan under QEMU microvm TCG: battery 10/10, goose (GLM over DNS + TLS), toolchain
  gate (rustc, `cargo build`).
- AArch64 under QEMU HVF (`MEMORY=2048M`, `SNAPSHOT=1`): the boot suite with no failures,
  `vda` registered, DHCP configured, ssh served, a 4 MiB file written and read back with
  the right md5, DNS resolved.
- amd64 under QEMU microvm TCG (`amd64/run.sh`): self-test 819 passed, 0 failed, including
  every `blk:` check (transport v2, capacity, ext2 superblock).
- Taoxichuan audit lines 92,404 -> 78,423. `akuma-virtio` unsafe sites 12 -> 18: the four
  `unsafe fn` receive/transmit contracts and the one `MmioWindow` claim that used to live
  inside `virtio-drivers`.

## Still open

- Firecracker (aarch64 and amd64) and kvm0 were not booted on the new driver. Both are v2
  and validate the status order, which the transport writes one milestone at a time.
- The rump tap (`RUMP_NIC=1`) and `net-noalloc` paths compile but were not run.
- virtio-sound still uses `virtio-drivers`. Moving it in-tree would drop the crate entirely.
