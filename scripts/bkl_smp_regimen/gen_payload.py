#!/usr/bin/env python3
"""Write the regimen's payload files and print their reference digests.

The payload is deterministic and non-compressible per 64 KiB block
(`AKUMA%07d` repeated), so a torn or dropped chunk cannot hide in zero-fill and
the block index that went wrong is recoverable from the corrupted bytes.

Usage: gen_payload.py <outdir>   — then serve <outdir> over HTTP on 127.0.0.1:8899
(the guest reaches the host as 10.0.2.2 under QEMU SLIRP).
"""
import hashlib
import os
import shutil
import sys

BLK = 64 * 1024
SIZES = {"p32.bin": 512, "p64.bin": 1024}  # in 64 KiB blocks


def write(path, blocks):
    h = hashlib.sha256()
    with open(path, "wb") as f:
        for i in range(blocks):
            b = (("AKUMA%07d" % i).encode() * (BLK // 9 + 1))[:BLK]
            f.write(b)
            h.update(b)
    return blocks * BLK, h.hexdigest()


def main(outdir):
    os.makedirs(outdir, exist_ok=True)
    lines = []
    for name, blocks in SIZES.items():
        size, digest = write(os.path.join(outdir, name), blocks)
        lines.append(f"{name} {size} {digest}")
        print(lines[-1])
    with open(os.path.join(outdir, "DIGESTS.txt"), "w") as f:
        f.write("\n".join(lines) + "\n")
    # job.sh is fetched by the VM from the same server.
    here = os.path.dirname(os.path.abspath(__file__))
    shutil.copy(os.path.join(here, "payload", "job.sh"), outdir)
    print(f"staged job.sh + payloads in {outdir}")
    print("REMINDER: job.sh embeds the reference digest — update it if the payload changes.")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "/tmp/bklpay")
