#!/usr/bin/env python3
"""Minimal read-only ext2 extractor: pull one file out of a disk image."""
import struct, sys

class Ext2:
    def __init__(self, path):
        self.f = open(path, 'rb')
        sb = self.pread(1024, 1024)
        self.inodes_count, self.blocks_count = struct.unpack_from('<II', sb, 0)
        log_bs, = struct.unpack_from('<I', sb, 24)
        self.bs = 1024 << log_bs
        self.blocks_per_group, = struct.unpack_from('<I', sb, 32)
        self.inodes_per_group, = struct.unpack_from('<I', sb, 40)
        rev, = struct.unpack_from('<I', sb, 76)
        self.inode_size = struct.unpack_from('<H', sb, 88)[0] if rev >= 1 else 128
        gd_block = 1 if self.bs == 1024 else 1
        self.gd_off = (gd_block + (1 if self.bs == 1024 else 0)) * self.bs
        self.gd_off = self.bs * (2 if self.bs == 1024 else 1)

    def pread(self, off, n):
        self.f.seek(off); return self.f.read(n)

    def block(self, n):
        return self.pread(n * self.bs, self.bs)

    def inode(self, ino):
        g = (ino - 1) // self.inodes_per_group
        i = (ino - 1) % self.inodes_per_group
        gd = self.pread(self.gd_off + g * 32, 32)
        itable, = struct.unpack_from('<I', gd, 8)
        raw = self.pread(itable * self.bs + i * self.inode_size, self.inode_size)
        mode, _uid, size_lo = struct.unpack_from('<HHI', raw, 0)
        blocks = struct.unpack_from('<15I', raw, 40)
        size_hi, = struct.unpack_from('<I', raw, 108)
        size = size_lo | (size_hi << 32) if (mode & 0xF000) == 0x8000 else size_lo
        return mode, size, blocks

    def block_list(self, blocks, need):
        """Yield data block numbers (classic direct/indirect, no extents)."""
        out = list(blocks[:12])
        ppb = self.bs // 4
        if len(out) < need and blocks[12]:
            out += list(struct.unpack_from('<%dI' % ppb, self.block(blocks[12]), 0))
        if len(out) < need and blocks[13]:
            for b in struct.unpack_from('<%dI' % ppb, self.block(blocks[13]), 0):
                if not b: break
                out += list(struct.unpack_from('<%dI' % ppb, self.block(b), 0))
                if len(out) >= need: break
        if len(out) < need and blocks[14]:
            for b1 in struct.unpack_from('<%dI' % ppb, self.block(blocks[14]), 0):
                if not b1: break
                for b2 in struct.unpack_from('<%dI' % ppb, self.block(b1), 0):
                    if not b2: break
                    out += list(struct.unpack_from('<%dI' % ppb, self.block(b2), 0))
                    if len(out) >= need: break
                if len(out) >= need: break
        return out[:need]

    def read_file(self, ino):
        mode, size, blocks = self.inode(ino)
        need = (size + self.bs - 1) // self.bs
        data = b''.join(self.block(b) if b else b'\0' * self.bs
                        for b in self.block_list(blocks, need))
        return data[:size]

    def readdir(self, ino):
        mode, size, blocks = self.inode(ino)
        need = (size + self.bs - 1) // self.bs
        ents = {}
        for bn in self.block_list(blocks, need):
            if not bn: continue
            b = self.block(bn); off = 0
            while off < len(b) - 8:
                child, rec, nlen, _t = struct.unpack_from('<IHBB', b, off)
                if rec < 8: break
                if child: ents[b[off+8:off+8+nlen].decode('latin1')] = child
                off += rec
        return ents

    def resolve(self, path):
        ino = 2
        for part in [p for p in path.split('/') if p]:
            ents = self.readdir(ino)
            if part not in ents: raise FileNotFoundError(f"{part} in {path}")
            ino = ents[part]
        return ino

if __name__ == '__main__':
    img, path, out = sys.argv[1], sys.argv[2], sys.argv[3]
    fs = Ext2(img)
    if path == '-ls':
        for k, v in sorted(fs.readdir(fs.resolve(out)).items()): print(v, k)
        sys.exit(0)
    ino = fs.resolve(path)
    data = fs.read_file(ino)
    open(out, 'wb').write(data)
    print(f"inode={ino} bytes={len(data)} -> {out}")
