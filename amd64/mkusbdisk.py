#!/usr/bin/env python3
"""Build a USB disk image shaped like the reference machine's spare drive.

The xHCI driver's self-test does not ask "is there a disk"; it asks four
questions about *this* disk — the MBR signature, `sda1` starting at LBA 2048,
an ext2 superblock 1024 bytes into it, and a scratch LBA inside `sda2`. A blank
image answers none of them, so the checks that matter most (the ones that read
and write real sectors) never run.

So the layout here is not decorative, it is the fixture:

    LBA 0            MBR, partition table, 0x55AA signature
    LBA 2048         sda1 — ext2, label AKUMA          (1 MiB in)
    LBA 134217728    sda2 — scratch, where WRITE(10) is exercised (64 GiB in)

The file is **sparse**: 64 GiB of declared size costs about the size of the
ext2 filesystem written into it. That is what makes a 64 GiB fixture something
you can keep in a scratch directory, and it is why the scratch LBA can stay at
its real-machine offset instead of being scaled down to fit — a scaled offset
would land inside `sda1` and the test would be writing over the filesystem it
just checked.

`mke2fs` is the one external tool. On macOS it is Homebrew's e2fsprogs, which
is keg-only, hence the explicit path search rather than a bare name.
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile

SECTOR = 512
P1_LBA = 2048  # sda1 — the `fdisk`/`mke2fs` default first partition
P2_LBA = 134_217_728  # sda2 — 64 GiB in, matching the real drive
SCRATCH_TAIL = 8192  # sectors of sda2 past its start, so the scratch LBA exists

MKE2FS_CANDIDATES = (
    "mke2fs",
    "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs",
    "/usr/local/opt/e2fsprogs/sbin/mke2fs",
    "/sbin/mke2fs",
    "/usr/sbin/mke2fs",
)


def find_mke2fs() -> str:
    for c in MKE2FS_CANDIDATES:
        p = shutil.which(c) if os.path.basename(c) == c else (c if os.path.exists(c) else None)
        if p:
            return p
    sys.exit(
        "mke2fs not found. Install e2fsprogs (macOS: `brew install e2fsprogs`, "
        "it is keg-only so this script looks in /opt/homebrew/opt/e2fsprogs/sbin)."
    )


def partition_entry(start_lba: int, sectors: int, ptype: int = 0x83) -> bytes:
    """One 16-byte MBR partition record.

    The CHS fields are the 0xFE/0xFF/0xFF "too big for CHS, use LBA" sentinel
    every modern tool writes. Nothing in this kernel reads them; a partition
    table that omitted them would still pass its own test and then look wrong
    to `fdisk` on the host, which is a bad trade for four bytes.
    """
    return struct.pack(
        "<B3sB3sII", 0x00, b"\xfe\xff\xff", ptype, b"\xfe\xff\xff", start_lba, sectors
    )


def build(path: str, ext2_mib: int) -> None:
    total_sectors = P2_LBA + SCRATCH_TAIL
    total_bytes = total_sectors * SECTOR

    # Sparse allocation: truncate declares the size without writing blocks.
    with open(path, "wb") as f:
        f.truncate(total_bytes)

    mbr = bytearray(SECTOR)
    mbr[446:462] = partition_entry(P1_LBA, P2_LBA - P1_LBA)
    mbr[462:478] = partition_entry(P2_LBA, total_sectors - P2_LBA)
    mbr[510:512] = b"\x55\xaa"
    with open(path, "r+b") as f:
        f.write(mbr)

    # Build the filesystem in its own file, then splice it in. mke2fs on a
    # sparse region of a 64 GiB file would want to size itself to the whole
    # thing; giving it an explicit block count in a separate file is simpler
    # than arguing with it about offsets.
    with tempfile.TemporaryDirectory() as tmp:
        part = os.path.join(tmp, "sda1.ext2")
        blocks = ext2_mib * 1024  # 1 KiB blocks
        subprocess.run(
            [find_mke2fs(), "-q", "-t", "ext2", "-b", "1024", "-L", "AKUMA", "-F", part,
             str(blocks)],
            check=True,
        )
        with open(part, "rb") as src, open(path, "r+b") as dst:
            dst.seek(P1_LBA * SECTOR)
            shutil.copyfileobj(src, dst, length=1 << 20)

    # Verify the fixture answers the questions the self-test will ask, here and
    # now — a fixture that is silently wrong turns into a driver bug hunt.
    with open(path, "rb") as f:
        f.seek(P1_LBA * SECTOR + 1024)
        magic = struct.unpack("<H", f.read(58)[56:58])[0]
    if magic != 0xEF53:
        sys.exit(f"ext2 superblock magic is 0x{magic:04x}, expected 0xef53")

    on_disk = os.stat(path).st_blocks * 512
    print(
        f"{path}: {total_bytes // 1024**3} GiB declared, "
        f"{on_disk // 1024**2} MiB actually allocated\n"
        f"  sda1 @ LBA {P1_LBA} — ext2 {ext2_mib} MiB, magic 0xEF53\n"
        f"  sda2 @ LBA {P2_LBA} — scratch"
    )


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("path", nargs="?", default="target/usbdisk.img")
    ap.add_argument("--ext2-mib", type=int, default=256,
                    help="size of the ext2 filesystem in sda1 (default 256)")
    ap.add_argument("--force", action="store_true", help="rebuild even if it exists")
    args = ap.parse_args()

    if os.path.exists(args.path) and not args.force:
        print(f"{args.path} exists; --force to rebuild")
        return
    os.makedirs(os.path.dirname(os.path.abspath(args.path)), exist_ok=True)
    build(args.path, args.ext2_mib)


if __name__ == "__main__":
    main()
