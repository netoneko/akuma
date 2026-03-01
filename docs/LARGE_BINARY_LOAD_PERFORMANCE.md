# Large Binary Load Performance

## Problem

Loading large binaries from ext2 (e.g. bun at 89 MB) is extremely slow. The I/O path has compounding inefficiencies at every layer: ELF loader, VFS, ext2, and block driver.

## Current I/O Path (bun, 89 MB)

```
ELF loader: ~22,700 read_at() calls (one per 4 KB page)
  └─ VFS read_at: path lookup + lock acquire per call
       └─ ext2 read_at: inode read + block-by-block loop
            └─ get_block_num: re-reads indirect blocks from disk every time
                 └─ read_block: allocates Vec, calls block::read_bytes
                      └─ read_bytes: allocates temp buf, reads sector-by-sector
                           └─ read_sectors: one VirtIO request per 512-byte sector
```

### Per-layer breakdown for one 4 KB ELF page (1024-byte ext2 blocks)

| Layer | Operations | Notes |
|-------|-----------|-------|
| ELF loader | 1 `read_at()` | 4 KB page granularity |
| VFS | 1 path resolve + string alloc | `with_fs()` resolves CWD, root_dir each call |
| ext2 `read_at` | 1 `lookup_path` + 1 `read_inode` + 4 block reads | Re-walks directory tree and re-reads inode every call |
| ext2 `get_block_num` | 4 indirect block reads | No cache; same metadata blocks re-read every time |
| ext2 `read_block` | 8 total block reads | 4 data + 4 indirect; each allocates a `Vec<u8>` |
| Block driver | 16 VirtIO sector reads | 2 sectors per 1024-byte block × 8 blocks |

### Total for 89 MB bun binary

| Metric | Count |
|--------|-------|
| `read_at()` calls | ~22,700 |
| Path lookups (directory walk) | ~22,700 |
| Inode reads from disk | ~22,700 |
| ext2 data block reads | ~91,000 |
| ext2 indirect/double-indirect reads | ~91,000 (uncached, redundant) |
| VirtIO sector requests | ~364,000 |
| Heap allocations (`Vec<u8>`) | ~200,000+ |

Most of the indirect block reads are redundant — the same few metadata blocks are re-read from disk hundreds or thousands of times.

## Solutions

### 1. Block cache (highest impact)

Add an LRU block cache to ext2. Indirect and double-indirect metadata blocks are read repeatedly for every single data block access. A cache of 256–512 entries would eliminate nearly all redundant disk reads.

**Scope:** `src/vfs/ext2.rs` — add a `BTreeMap<u32, Vec<u8>>` or small LRU cache checked in `read_block()`.

**Estimate:** ~150 lines. 2–3 hours.

**Impact:** Eliminates ~91,000 redundant block reads (the indirect/double-indirect metadata reads). Cuts total VirtIO requests roughly in half. This is the single biggest win because every data block access currently re-reads 1–2 metadata blocks from disk.

### 2. Batch VirtIO sector reads

`read_sectors()` currently issues one `inner.read_blocks()` call per 512-byte sector in a loop. The VirtIO block spec supports multi-sector requests.

**Scope:** `src/block.rs` — change `read_sectors()` to pass the full buffer to a single `read_blocks()` call (if the virtio-drivers crate supports it), or batch into larger requests.

**Estimate:** ~30 lines. 1–2 hours (depends on virtio-drivers API).

**Impact:** Reduces VirtIO overhead by up to 8× per block read. Combined with a block cache, this makes remaining disk reads much faster.

### 3. Larger ELF read granularity

The ELF loader reads one 4 KB page at a time via `read_at()`. Each call re-resolves the path, re-reads the inode, and re-walks indirect blocks from scratch. Reading in 64 KB or 256 KB chunks would reduce the number of `read_at()` calls by 16–64×.

**Scope:** `src/elf_loader.rs` — in `load_elf_from_path()`, pre-read segment data in large chunks (e.g. 256 KB) and copy into pages from the buffer.

**Estimate:** ~80 lines. 2–3 hours.

**Impact:** Reduces `read_at()` calls from ~22,700 to ~350 (at 256 KB). Eliminates ~22,000 redundant path lookups and inode reads. Significant even with a block cache, because path resolution and lock acquisition have fixed overhead.

### 4. Use 4096-byte ext2 blocks

`create_disk.sh` runs `mkfs.ext2 -F` without `-b`, defaulting to 1024-byte blocks on a 256 MB image. Using `-b 4096` would reduce block operations by 4× and make indirect block tables cover 4× more data (256 entries → 1024 entries per indirect block).

**Scope:** `scripts/create_disk.sh` — add `-b 4096` to all `mkfs.ext2` invocations. Requires recreating the disk image.

**Estimate:** ~5 lines. 15 minutes. Requires `scripts/create_disk.sh` + `scripts/populate_disk.sh` re-run.

**Impact:** 4× fewer block reads per file access. Single-indirect blocks cover 4 MB instead of 256 KB, so most of bun's data falls within single-indirect range rather than requiring double-indirect lookups.

### 5. Open file handle with cached inode

Currently, every `read_at()` call does a full `lookup_path()` (directory tree walk) and `read_inode()` from disk. An open-file abstraction that caches the resolved inode number would eliminate this repeated work.

**Scope:** `src/vfs/ext2.rs` and `src/elf_loader.rs` — add a `read_at_inode()` method that takes a pre-resolved inode number, and have the ELF loader resolve once then use the inode directly.

**Estimate:** ~60 lines. 1–2 hours.

**Impact:** Eliminates ~22,700 directory walks and inode reads. Moderate standalone impact, but compounds well with larger read granularity.

### 6. Raise the in-memory file size limit

The current 16 MB limit in `read_file()` forces large binaries into the slow page-by-page path. Raising this to 128 MB (with the 120 MB heap, feasible for files up to ~100 MB) would let bun load via the fast single-read path.

**Scope:** `src/vfs/ext2.rs` — change the 16 MB cap. Risky for memory pressure.

**Estimate:** ~5 lines. 15 minutes.

**Impact:** Eliminates the page-by-page overhead entirely for files under the new limit. However, risks OOM for multiple concurrent large loads. Better as a complement to other fixes rather than standalone.

## Recommended Implementation Order

| Priority | Fix | Effort | Estimated Speedup |
|----------|-----|--------|-------------------|
| 1 | Block cache | 2–3 hours | ~2–3× |
| 2 | 4096-byte ext2 blocks | 15 min | ~2–4× |
| 3 | Larger ELF read chunks | 2–3 hours | ~2× |
| 4 | Batch VirtIO sectors | 1–2 hours | ~1.5–2× |
| 5 | Cached inode in read_at | 1–2 hours | ~1.3× |
| 6 | Raise file size limit | 15 min | large but risky |

Fixes 1–4 together should reduce bun's load time by roughly **10–20×**. The total effort is approximately 6–8 hours.
