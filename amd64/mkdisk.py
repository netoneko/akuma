#!/usr/bin/env python3
"""Build the amd64 target's probe disk.

Not an ext2 image and not a filesystem of any kind — a raw disk with two known
sectors on it, so `amd64/src/blk.rs` can prove the whole path from a descriptor
ring to the device's view of memory rather than proving that a driver object was
constructed.

Two sectors, not one, and 1 MiB apart on purpose: a driver that ignored the
requested offset and returned sector 0 for every request would satisfy every
check made against a single sector.

The pattern after each signature is `(i * 7 + 3) & 0xff` — position-dependent, so
a short DMA or an off-by-one descriptor length shows up as a mismatch at the
first byte that was not transferred, rather than as a buffer that still looks
plausible.
"""

import sys

SECTOR = 512
SIG_0 = b"AKUMA/amd64 blk probe"
SIG_FAR = b"AKUMA/amd64 far sector"
FAR_LBA = 2048  # 1 MiB in


def patterned(signature: bytes) -> bytes:
    """One sector: a signature, then a position-dependent pattern."""
    out = bytearray(SECTOR)
    out[: len(signature)] = signature
    for i in range(len(signature), SECTOR):
        out[i] = (i * 7 + 3) & 0xFF
    return bytes(out)


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "amd64-probe.img"
    size_mib = int(sys.argv[2]) if len(sys.argv) > 2 else 4

    image = bytearray(size_mib * 1024 * 1024)
    image[0:SECTOR] = patterned(SIG_0)
    image[FAR_LBA * SECTOR : (FAR_LBA + 1) * SECTOR] = patterned(SIG_FAR)

    with open(path, "wb") as f:
        f.write(image)
    print(f"{path}: {size_mib} MiB, sector 0 and sector {FAR_LBA} written")
    return 0


if __name__ == "__main__":
    sys.exit(main())
