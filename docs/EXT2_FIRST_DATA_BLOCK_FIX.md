# ext2 `first_data_block` Off-By-One Fix

## Problem

`pkg install scratch` (422,272 bytes) failed with:

```
Error: Failed to write to /bin/ (422272 bytes): I/O error
```

Smaller binaries (< 90KB) installed fine. The issue persisted after fixing TCP flow control for smoltcp.

## Root Cause

`allocate_block` computed block numbers as:

```rust
let block_num = group * blocks_per_group + bit;
```

The ext2 spec requires:

```rust
let block_num = first_data_block + group * blocks_per_group + bit;
```

For 1024-byte block filesystems, `first_data_block = 1` (block 0 is the boot sector). Every allocated block number was off by 1.

### Why This Corrupts the Inode Table

In each block group, the block bitmap's first "free" data bit (after metadata) maps — in the incorrect scheme — to the **last block of the inode table**, not the first free data block:

| Bit | Correct block | Akuma (buggy) block | Actual content |
|-----|---------------|---------------------|----------------|
| 772 | 773 (free)    | 772                 | inode table!   |

File data written to block 772 overwrites inodes. Confirmed on disk: group 1's last inode table block (8964) contained garbage — pseudo-inodes with `direct_blocks` pointers like 3,584,891,721, mapping to sectors far beyond the 262,144-sector disk.

### Why Large Files Trigger the Error

Small files allocate from one group. The first allocation corrupts one inode table block, but the file itself writes and reads consistently (the off-by-one is symmetric). Large files (>268KB with 1024-byte blocks) need double-indirect blocks, requiring 400+ allocations spanning multiple groups. Operations that touch corrupted inodes in other groups hit out-of-bounds block pointers, causing VirtIO I/O errors.

## Fix

In `src/vfs/ext2.rs`:

1. **`allocate_block`**: `block_num = state.first_data_block + group * blocks_per_group + bit`
2. **`free_block`**: `adjusted = block_num - state.first_data_block` before computing group/bit
3. **`block_group_count`**: `(total_blocks - first_data_block + blocks_per_group - 1) / blocks_per_group`
4. **`bgd_offset`**: uses `first_data_block + 1` instead of hardcoded block number
5. **`Ext2State`**: stores `first_data_block` from superblock

## Recovery

The existing disk image has corrupted inode tables and must be recreated:

```bash
./scripts/create_disk.sh
./scripts/populate_disk.sh
```
