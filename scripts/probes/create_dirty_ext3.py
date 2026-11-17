#!/usr/bin/env python3
"""Create a disposable ext3 image with one committed JBD transaction.

This independently constructs the documented on-disk records; it does not use
filesystem implementation code. The guest must observe the replayed contents.
"""
import os
import struct
import subprocess
import sys

image = sys.argv[1]
payload = image + ".payload"
with open(image, "wb") as f:
    f.truncate(32 * 1024 * 1024)
with open(payload, "wb") as f:
    f.write(b"QEMU_EXT4_READ_OK\n")
subprocess.run(["mke2fs", "-q", "-F", "-t", "ext3", "-b", "1024", image], check=True)
subprocess.run(["debugfs", "-w", "-R", f"write {payload} /payload", image], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)

def bmap(command):
    result = subprocess.run(["debugfs", "-R", command, image], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=True, text=True)
    return int(result.stdout.strip())

home = bmap("bmap /payload 0")
journal = [bmap(f"bmap <8> {i}") for i in range(4)]
with open(image, "r+b") as f:
    f.seek(1024)
    sb = bytearray(f.read(1024))
    uuid = sb[104:120]
    struct.pack_into("<I", sb, 96, struct.unpack_from("<I", sb, 96)[0] | 4)
    f.seek(1024)
    f.write(sb)
    def block(number, data):
        f.seek(number * 1024)
        f.write(data)
    f.seek(journal[0] * 1024)
    js = bytearray(f.read(1024))
    struct.pack_into(">II", js, 24, 1, 1)
    block(journal[0], js)
    descriptor = bytearray(1024)
    struct.pack_into(">IIIII", descriptor, 0, 0xC03B3998, 1, 1, home, 8)
    descriptor[20:36] = uuid
    block(journal[1], descriptor)
    data = bytearray(1024)
    data[:18] = b"QEMU_EXT3_REPLAY!\n"
    block(journal[2], data)
    commit = bytearray(1024)
    struct.pack_into(">III", commit, 0, 0xC03B3998, 2, 1)
    block(journal[3], commit)
os.unlink(payload)
