//! Ext2 Filesystem Implementation
//!
//! A full ext2 filesystem driver for no_std environments with read/write support.
//! Based on the ext2 specification and inspired by the mikros ext2 implementation.
//! Reference: https://gitea.pterpstra.com/mikros/ext2

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use spinning_top::Spinlock;

use super::{DirEntry, Filesystem, FsError, FsStats, Metadata, path_components, split_path};
use crate::block;
use crate::console;

// ============================================================================
// Constants
// ============================================================================

/// Ext2 superblock magic number
const EXT2_MAGIC: u16 = 0xEF53;

/// Superblock offset from start of disk (always 1024 bytes)
const SUPERBLOCK_OFFSET: u64 = 1024;

/// Root directory inode number
const ROOT_INODE: u32 = 2;

/// File type constants (from inode type_perms field)
const S_IFREG: u16 = 0x8000; // Regular file
const S_IFDIR: u16 = 0x4000; // Directory

/// Default permissions for new files/directories
const DEFAULT_FILE_PERMS: u16 = S_IFREG | 0o644;
const DEFAULT_DIR_PERMS: u16 = S_IFDIR | 0o755;

/// Directory entry file type constants
const FT_REG_FILE: u8 = 1;
const FT_DIR: u8 = 2;

/// Minimum directory entry size (inode + rec_len + name_len + file_type)
const DIR_ENTRY_HEADER_SIZE: usize = 8;

// ============================================================================
// On-disk Structures
// ============================================================================

/// Ext2 Superblock (located at byte offset 1024)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct Superblock {
    total_inodes: u32,
    total_blocks: u32,
    superuser_blocks: u32,
    unallocated_blocks: u32,
    unallocated_inodes: u32,
    superblock_block: u32,
    block_size_log: u32,
    fragment_size_log: u32,
    blocks_per_group: u32,
    fragments_per_group: u32,
    inodes_per_group: u32,
    last_mount_time: u32,
    last_written_time: u32,
    mount_count: u16,
    max_mount_count: u16,
    magic: u16,
    fs_state: u16,
    error_handling: u16,
    version_minor: u16,
    last_check_time: u32,
    check_interval: u32,
    creator_os: u32,
    version_major: u32,
    reserved_uid: u16,
    reserved_gid: u16,
    first_inode: u32,
    inode_size: u16,
    block_group: u16,
    feature_compat: u32,
    feature_incompat: u32,
    feature_ro_compat: u32,
    uuid: [u8; 16],
    volume_name: [u8; 16],
    last_mounted: [u8; 64],
    algo_bitmap: u32,
    _padding: [u8; 820],
}

/// Block Group Descriptor
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct BlockGroupDescriptor {
    block_bitmap: u32,
    inode_bitmap: u32,
    inode_table: u32,
    free_blocks_count: u16,
    free_inodes_count: u16,
    used_dirs_count: u16,
    _padding: u16,
    _reserved: [u8; 12],
}

/// Inode structure
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct Inode {
    type_perms: u16,
    uid: u16,
    size_lower: u32,
    access_time: u32,
    creation_time: u32,
    modification_time: u32,
    deletion_time: u32,
    gid: u16,
    hard_links: u16,
    sectors_used: u32,
    flags: u32,
    os_specific_1: u32,
    direct_blocks: [u32; 12],
    indirect_block: u32,
    double_indirect_block: u32,
    triple_indirect_block: u32,
    generation: u32,
    file_acl: u32,
    size_upper: u32,
    fragment_addr: u32,
    os_specific_2: [u8; 12],
}

impl Default for Inode {
    fn default() -> Self {
        Self {
            type_perms: 0,
            uid: 0,
            size_lower: 0,
            access_time: 0,
            creation_time: 0,
            modification_time: 0,
            deletion_time: 0,
            gid: 0,
            hard_links: 0,
            sectors_used: 0,
            flags: 0,
            os_specific_1: 0,
            direct_blocks: [0; 12],
            indirect_block: 0,
            double_indirect_block: 0,
            triple_indirect_block: 0,
            generation: 0,
            file_acl: 0,
            size_upper: 0,
            fragment_addr: 0,
            os_specific_2: [0; 12],
        }
    }
}

/// Directory entry (variable size on disk)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct DirEntryRaw {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
}

// ============================================================================
// Ext2 Filesystem State
// ============================================================================

struct Ext2State {
    superblock: Superblock,
    block_size: usize,
    inodes_per_group: u32,
    inode_size: u16,
    block_group_count: u32,
    blocks_per_group: u32,
}

// ============================================================================
// Ext2 Filesystem Implementation
// ============================================================================

/// Ext2 filesystem implementation
pub struct Ext2Filesystem {
    state: Spinlock<Ext2State>,
}

impl Ext2Filesystem {
    /// Create a new Ext2 filesystem from the block device
    pub fn new() -> Result<Self, FsError> {
        log("[Ext2] Mounting ext2 filesystem...\n");

        let mut sb_buf = [0u8; 1024];
        block::read_bytes(SUPERBLOCK_OFFSET, &mut sb_buf).map_err(|_| FsError::IoError)?;

        let superblock: Superblock =
            unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const _) };

        let magic = superblock.magic;
        let block_size_log = superblock.block_size_log;
        let version_major = superblock.version_major;
        let sb_inode_size = superblock.inode_size;
        let total_blocks = superblock.total_blocks;
        let total_inodes = superblock.total_inodes;
        let blocks_per_group = superblock.blocks_per_group;
        let inodes_per_group = superblock.inodes_per_group;

        if magic != EXT2_MAGIC {
            crate::safe_print!(96, 
                "[Ext2] Invalid magic: 0x{:04X} (expected 0x{:04X})\n",
                magic,
                EXT2_MAGIC
            );
            return Err(FsError::NoFilesystem);
        }

        let block_size = 1024usize << block_size_log;
        let inode_size = if version_major >= 1 {
            sb_inode_size
        } else {
            128
        };
        let block_group_count = (total_blocks + blocks_per_group - 1) / blocks_per_group;

        crate::safe_print!(160, 
            "[Ext2] Mounted: {} blocks, {} inodes, {} byte blocks, {} groups\n",
            total_blocks,
            total_inodes,
            block_size,
            block_group_count
        );

        let state = Ext2State {
            superblock,
            block_size,
            inodes_per_group,
            inode_size,
            block_group_count,
            blocks_per_group,
        };

        Ok(Self {
            state: Spinlock::new(state),
        })
    }

    /// Mount ext2 and return a boxed Filesystem trait object
    pub fn mount() -> Result<Box<dyn Filesystem>, FsError> {
        Ok(Box::new(Self::new()?))
    }

    // ========================================================================
    // Block I/O
    // ========================================================================

    fn read_block(state: &Ext2State, block_num: u32) -> Result<Vec<u8>, FsError> {
        let mut buf = vec![0u8; state.block_size];
        let offset = block_num as u64 * state.block_size as u64;
        block::read_bytes(offset, &mut buf).map_err(|_| FsError::IoError)?;
        Ok(buf)
    }

    fn write_block(state: &Ext2State, block_num: u32, data: &[u8]) -> Result<(), FsError> {
        if data.len() != state.block_size {
            return Err(FsError::Internal);
        }
        let offset = block_num as u64 * state.block_size as u64;
        block::write_bytes(offset, data).map_err(|_| FsError::IoError)?;
        Ok(())
    }

    // ========================================================================
    // Superblock Management
    // ========================================================================

    fn write_superblock(state: &Ext2State) -> Result<(), FsError> {
        let buf = unsafe {
            core::slice::from_raw_parts(
                &state.superblock as *const Superblock as *const u8,
                size_of::<Superblock>(),
            )
        };
        block::write_bytes(SUPERBLOCK_OFFSET, buf).map_err(|_| FsError::IoError)?;
        Ok(())
    }

    // ========================================================================
    // Block Group Descriptor Management
    // ========================================================================

    fn bgd_offset(state: &Ext2State, group: u32) -> u64 {
        let bgd_table_block = if state.block_size == 1024 { 2 } else { 1 };
        bgd_table_block as u64 * state.block_size as u64
            + group as u64 * size_of::<BlockGroupDescriptor>() as u64
    }

    fn read_bgd(state: &Ext2State, group: u32) -> Result<BlockGroupDescriptor, FsError> {
        let offset = Self::bgd_offset(state, group);
        let mut buf = [0u8; size_of::<BlockGroupDescriptor>()];
        block::read_bytes(offset, &mut buf).map_err(|_| FsError::IoError)?;
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) })
    }

    fn write_bgd(state: &Ext2State, group: u32, bgd: &BlockGroupDescriptor) -> Result<(), FsError> {
        let offset = Self::bgd_offset(state, group);
        let buf = unsafe {
            core::slice::from_raw_parts(
                bgd as *const BlockGroupDescriptor as *const u8,
                size_of::<BlockGroupDescriptor>(),
            )
        };
        block::write_bytes(offset, buf).map_err(|_| FsError::IoError)?;
        Ok(())
    }

    // ========================================================================
    // Inode Management
    // ========================================================================

    fn read_inode(state: &Ext2State, inode_num: u32) -> Result<Inode, FsError> {
        if inode_num == 0 {
            return Err(FsError::NotFound);
        }
        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let index_in_group = inode_idx % state.inodes_per_group;

        let bgd = Self::read_bgd(state, group)?;
        let inode_table = bgd.inode_table;

        let inode_offset = inode_table as u64 * state.block_size as u64
            + index_in_group as u64 * state.inode_size as u64;

        let mut buf = vec![0u8; state.inode_size as usize];
        block::read_bytes(inode_offset, &mut buf).map_err(|_| FsError::IoError)?;

        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) })
    }

    fn write_inode(state: &Ext2State, inode_num: u32, inode: &Inode) -> Result<(), FsError> {
        if inode_num == 0 {
            return Err(FsError::NotFound);
        }
        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let index_in_group = inode_idx % state.inodes_per_group;

        let bgd = Self::read_bgd(state, group)?;
        let inode_table = bgd.inode_table;

        let inode_offset = inode_table as u64 * state.block_size as u64
            + index_in_group as u64 * state.inode_size as u64;

        let buf = unsafe {
            core::slice::from_raw_parts(inode as *const Inode as *const u8, size_of::<Inode>())
        };
        block::write_bytes(inode_offset, buf).map_err(|_| FsError::IoError)?;
        Ok(())
    }

    // ========================================================================
    // Bitmap Operations
    // ========================================================================

    fn get_bit(bitmap: &[u8], bit: u32) -> bool {
        let byte = bit / 8;
        let bit_offset = bit % 8;
        if (byte as usize) < bitmap.len() {
            (bitmap[byte as usize] & (1 << bit_offset)) != 0
        } else {
            true // Out of range = allocated
        }
    }

    fn set_bit(bitmap: &mut [u8], bit: u32, value: bool) {
        let byte = bit / 8;
        let bit_offset = bit % 8;
        if (byte as usize) < bitmap.len() {
            if value {
                bitmap[byte as usize] |= 1 << bit_offset;
            } else {
                bitmap[byte as usize] &= !(1 << bit_offset);
            }
        }
    }

    // ========================================================================
    // Block Allocation
    // ========================================================================

    fn allocate_block(state: &mut Ext2State) -> Result<u32, FsError> {
        let unalloc = state.superblock.unallocated_blocks;
        if unalloc == 0 {
            return Err(FsError::NoSpace);
        }

        for group in 0..state.block_group_count {
            let mut bgd = Self::read_bgd(state, group)?;
            let free_count = bgd.free_blocks_count;
            if free_count == 0 {
                continue;
            }

            let bitmap_block = bgd.block_bitmap;
            let mut bitmap = Self::read_block(state, bitmap_block)?;

            // Find first free bit
            for bit in 0..state.blocks_per_group {
                if !Self::get_bit(&bitmap, bit) {
                    // Found free block
                    Self::set_bit(&mut bitmap, bit, true);
                    Self::write_block(state, bitmap_block, &bitmap)?;

                    // Update BGD
                    bgd.free_blocks_count = free_count - 1;
                    Self::write_bgd(state, group, &bgd)?;

                    // Update superblock
                    state.superblock.unallocated_blocks = unalloc - 1;
                    Self::write_superblock(state)?;

                    let block_num = group * state.blocks_per_group + bit;

                    // Zero the block
                    let zeros = vec![0u8; state.block_size];
                    Self::write_block(state, block_num, &zeros)?;

                    return Ok(block_num);
                }
            }
        }

        Err(FsError::NoSpace)
    }

    fn free_block(state: &mut Ext2State, block_num: u32) -> Result<(), FsError> {
        if block_num == 0 {
            return Ok(());
        }

        let group = block_num / state.blocks_per_group;
        let bit = block_num % state.blocks_per_group;

        let mut bgd = Self::read_bgd(state, group)?;
        let bitmap_block = bgd.block_bitmap;
        let mut bitmap = Self::read_block(state, bitmap_block)?;

        Self::set_bit(&mut bitmap, bit, false);
        Self::write_block(state, bitmap_block, &bitmap)?;

        bgd.free_blocks_count += 1;
        Self::write_bgd(state, group, &bgd)?;

        state.superblock.unallocated_blocks += 1;
        Self::write_superblock(state)?;

        Ok(())
    }

    // ========================================================================
    // Inode Allocation
    // ========================================================================

    fn allocate_inode(state: &mut Ext2State, is_dir: bool) -> Result<u32, FsError> {
        let unalloc = state.superblock.unallocated_inodes;
        if unalloc == 0 {
            return Err(FsError::NoSpace);
        }

        for group in 0..state.block_group_count {
            let mut bgd = Self::read_bgd(state, group)?;
            let free_count = bgd.free_inodes_count;
            if free_count == 0 {
                continue;
            }

            let bitmap_block = bgd.inode_bitmap;
            let mut bitmap = Self::read_block(state, bitmap_block)?;

            for bit in 0..state.inodes_per_group {
                if !Self::get_bit(&bitmap, bit) {
                    Self::set_bit(&mut bitmap, bit, true);
                    Self::write_block(state, bitmap_block, &bitmap)?;

                    bgd.free_inodes_count = free_count - 1;
                    if is_dir {
                        bgd.used_dirs_count += 1;
                    }
                    Self::write_bgd(state, group, &bgd)?;

                    state.superblock.unallocated_inodes = unalloc - 1;
                    Self::write_superblock(state)?;

                    let inode_num = group * state.inodes_per_group + bit + 1;
                    return Ok(inode_num);
                }
            }
        }

        Err(FsError::NoSpace)
    }

    fn free_inode(state: &mut Ext2State, inode_num: u32, is_dir: bool) -> Result<(), FsError> {
        if inode_num == 0 {
            return Ok(());
        }

        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let bit = inode_idx % state.inodes_per_group;

        let mut bgd = Self::read_bgd(state, group)?;
        let bitmap_block = bgd.inode_bitmap;
        let mut bitmap = Self::read_block(state, bitmap_block)?;

        Self::set_bit(&mut bitmap, bit, false);
        Self::write_block(state, bitmap_block, &bitmap)?;

        bgd.free_inodes_count += 1;
        if is_dir && bgd.used_dirs_count > 0 {
            bgd.used_dirs_count -= 1;
        }
        Self::write_bgd(state, group, &bgd)?;

        state.superblock.unallocated_inodes += 1;
        Self::write_superblock(state)?;

        Ok(())
    }

    // ========================================================================
    // Block Mapping (logical -> physical)
    // ========================================================================

    fn get_block_num(
        state: &Ext2State,
        inode: &Inode,
        logical_block: u32,
    ) -> Result<Option<u32>, FsError> {
        let ptrs_per_block = (state.block_size / 4) as u32;

        if logical_block < 12 {
            let block = inode.direct_blocks[logical_block as usize];
            return Ok(if block == 0 { None } else { Some(block) });
        }

        let logical_block = logical_block - 12;

        if logical_block < ptrs_per_block {
            if inode.indirect_block == 0 {
                return Ok(None);
            }
            let indirect = Self::read_block(state, inode.indirect_block)?;
            let block = Self::read_block_ptr(&indirect, logical_block as usize);
            return Ok(if block == 0 { None } else { Some(block) });
        }

        let logical_block = logical_block - ptrs_per_block;

        if logical_block < ptrs_per_block * ptrs_per_block {
            if inode.double_indirect_block == 0 {
                return Ok(None);
            }
            let idx1 = (logical_block / ptrs_per_block) as usize;
            let idx2 = (logical_block % ptrs_per_block) as usize;

            let double_indirect = Self::read_block(state, inode.double_indirect_block)?;
            let indirect_block = Self::read_block_ptr(&double_indirect, idx1);
            if indirect_block == 0 {
                return Ok(None);
            }

            let indirect = Self::read_block(state, indirect_block)?;
            let block = Self::read_block_ptr(&indirect, idx2);
            return Ok(if block == 0 { None } else { Some(block) });
        }

        Err(FsError::NotSupported)
    }

    fn read_block_ptr(block: &[u8], index: usize) -> u32 {
        let offset = index * 4;
        u32::from_le_bytes([
            block[offset],
            block[offset + 1],
            block[offset + 2],
            block[offset + 3],
        ])
    }

    fn write_block_ptr(block: &mut [u8], index: usize, value: u32) {
        let offset = index * 4;
        let bytes = value.to_le_bytes();
        block[offset..offset + 4].copy_from_slice(&bytes);
    }

    /// Ensure a block exists at the given logical position, allocating if needed
    fn ensure_block(
        state: &mut Ext2State,
        inode: &mut Inode,
        logical_block: u32,
    ) -> Result<u32, FsError> {
        let ptrs_per_block = (state.block_size / 4) as u32;

        if logical_block < 12 {
            if inode.direct_blocks[logical_block as usize] == 0 {
                let new_block = Self::allocate_block(state)?;
                inode.direct_blocks[logical_block as usize] = new_block;
                inode.sectors_used += (state.block_size / 512) as u32;
            }
            return Ok(inode.direct_blocks[logical_block as usize]);
        }

        let lb = logical_block - 12;

        if lb < ptrs_per_block {
            // Singly indirect
            if inode.indirect_block == 0 {
                inode.indirect_block = Self::allocate_block(state)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            let mut indirect = Self::read_block(state, inode.indirect_block)?;
            let mut block = Self::read_block_ptr(&indirect, lb as usize);

            if block == 0 {
                block = Self::allocate_block(state)?;
                Self::write_block_ptr(&mut indirect, lb as usize, block);
                Self::write_block(state, inode.indirect_block, &indirect)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            return Ok(block);
        }

        let lb = lb - ptrs_per_block;

        if lb < ptrs_per_block * ptrs_per_block {
            // Doubly indirect
            if inode.double_indirect_block == 0 {
                inode.double_indirect_block = Self::allocate_block(state)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            let idx1 = (lb / ptrs_per_block) as usize;
            let idx2 = (lb % ptrs_per_block) as usize;

            let mut double_indirect = Self::read_block(state, inode.double_indirect_block)?;
            let mut indirect_block = Self::read_block_ptr(&double_indirect, idx1);

            if indirect_block == 0 {
                indirect_block = Self::allocate_block(state)?;
                Self::write_block_ptr(&mut double_indirect, idx1, indirect_block);
                Self::write_block(state, inode.double_indirect_block, &double_indirect)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            let mut indirect = Self::read_block(state, indirect_block)?;
            let mut block = Self::read_block_ptr(&indirect, idx2);

            if block == 0 {
                block = Self::allocate_block(state)?;
                Self::write_block_ptr(&mut indirect, idx2, block);
                Self::write_block(state, indirect_block, &indirect)?;
                inode.sectors_used += (state.block_size / 512) as u32;
            }

            return Ok(block);
        }

        Err(FsError::NotSupported)
    }

    // ========================================================================
    // Inode Data Operations
    // ========================================================================

    fn read_inode_data(state: &Ext2State, inode: &Inode) -> Result<Vec<u8>, FsError> {
        let size = inode.size_lower as usize;
        let mut data = Vec::with_capacity(size);
        let blocks_needed = (size + state.block_size - 1) / state.block_size;

        for logical_block in 0..blocks_needed as u32 {
            if let Some(phys_block) = Self::get_block_num(state, inode, logical_block)? {
                let block_data = Self::read_block(state, phys_block)?;
                let remaining = size - data.len();
                let to_copy = core::cmp::min(remaining, state.block_size);
                data.extend_from_slice(&block_data[..to_copy]);
            } else {
                let remaining = size - data.len();
                let to_copy = core::cmp::min(remaining, state.block_size);
                data.extend(core::iter::repeat(0).take(to_copy));
            }
        }

        Ok(data)
    }

    fn write_inode_data(
        state: &mut Ext2State,
        inode_num: u32,
        inode: &mut Inode,
        data: &[u8],
    ) -> Result<(), FsError> {
        let blocks_needed = (data.len() + state.block_size - 1) / state.block_size;

        for logical_block in 0..blocks_needed as u32 {
            let phys_block = Self::ensure_block(state, inode, logical_block)?;

            let start = logical_block as usize * state.block_size;
            let end = core::cmp::min(start + state.block_size, data.len());

            let mut block_data = vec![0u8; state.block_size];
            block_data[..end - start].copy_from_slice(&data[start..end]);

            Self::write_block(state, phys_block, &block_data)?;
        }

        inode.size_lower = data.len() as u32;
        let now = current_time();
        inode.modification_time = now;
        Self::write_inode(state, inode_num, inode)?;

        Ok(())
    }

    fn truncate_inode(state: &mut Ext2State, inode: &mut Inode) -> Result<(), FsError> {
        // Free all direct blocks
        for i in 0..12 {
            if inode.direct_blocks[i] != 0 {
                Self::free_block(state, inode.direct_blocks[i])?;
                inode.direct_blocks[i] = 0;
            }
        }

        // Free indirect block and its contents
        if inode.indirect_block != 0 {
            let ptrs_per_block = state.block_size / 4;
            let indirect = Self::read_block(state, inode.indirect_block)?;
            for i in 0..ptrs_per_block {
                let block = Self::read_block_ptr(&indirect, i);
                if block != 0 {
                    Self::free_block(state, block)?;
                }
            }
            Self::free_block(state, inode.indirect_block)?;
            inode.indirect_block = 0;
        }

        // Free double indirect (simplified - just free the pointer block)
        if inode.double_indirect_block != 0 {
            let ptrs_per_block = state.block_size / 4;
            let double_indirect = Self::read_block(state, inode.double_indirect_block)?;
            for i in 0..ptrs_per_block {
                let indirect_block = Self::read_block_ptr(&double_indirect, i);
                if indirect_block != 0 {
                    let indirect = Self::read_block(state, indirect_block)?;
                    for j in 0..ptrs_per_block {
                        let block = Self::read_block_ptr(&indirect, j);
                        if block != 0 {
                            Self::free_block(state, block)?;
                        }
                    }
                    Self::free_block(state, indirect_block)?;
                }
            }
            Self::free_block(state, inode.double_indirect_block)?;
            inode.double_indirect_block = 0;
        }

        inode.size_lower = 0;
        inode.sectors_used = 0;

        Ok(())
    }

    // ========================================================================
    // Directory Operations
    // ========================================================================

    fn parse_directory(data: &[u8]) -> Vec<(u32, String, u8)> {
        let mut entries = Vec::new();
        let mut offset = 0;

        while offset + DIR_ENTRY_HEADER_SIZE <= data.len() {
            let entry: DirEntryRaw =
                unsafe { core::ptr::read_unaligned(data[offset..].as_ptr() as *const _) };

            if entry.rec_len == 0 {
                break;
            }

            if entry.inode != 0 {
                let name_start = offset + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + entry.name_len as usize;

                if name_end <= data.len() {
                    if let Ok(name) = core::str::from_utf8(&data[name_start..name_end]) {
                        entries.push((entry.inode, name.to_string(), entry.file_type));
                    }
                }
            }

            offset += entry.rec_len as usize;
        }

        entries
    }

    fn add_dir_entry(
        state: &mut Ext2State,
        dir_inode_num: u32,
        name: &str,
        inode_num: u32,
        file_type: u8,
    ) -> Result<(), FsError> {
        let mut dir_inode = Self::read_inode(state, dir_inode_num)?;
        let mut dir_data = Self::read_inode_data(state, &dir_inode)?;

        let name_bytes = name.as_bytes();
        let needed_len = DIR_ENTRY_HEADER_SIZE + name_bytes.len();
        let aligned_len = (needed_len + 3) & !3; // Align to 4 bytes

        // Try to find space in existing entries
        let mut offset = 0;
        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let entry: DirEntryRaw =
                unsafe { core::ptr::read_unaligned(dir_data[offset..].as_ptr() as *const _) };

            if entry.rec_len == 0 {
                break;
            }

            let actual_len = if entry.inode == 0 {
                0
            } else {
                (DIR_ENTRY_HEADER_SIZE + entry.name_len as usize + 3) & !3
            };

            let free_space = entry.rec_len as usize - actual_len;

            if free_space >= aligned_len {
                // Split this entry
                if entry.inode != 0 {
                    // Shrink existing entry
                    let new_rec_len = actual_len as u16;
                    dir_data[offset + 4] = new_rec_len as u8;
                    dir_data[offset + 5] = (new_rec_len >> 8) as u8;

                    offset += actual_len;
                }

                // Write new entry
                let new_entry = DirEntryRaw {
                    inode: inode_num,
                    rec_len: (entry.rec_len as usize - actual_len) as u16,
                    name_len: name_bytes.len() as u8,
                    file_type,
                };

                let entry_bytes = unsafe {
                    core::slice::from_raw_parts(
                        &new_entry as *const DirEntryRaw as *const u8,
                        DIR_ENTRY_HEADER_SIZE,
                    )
                };
                dir_data[offset..offset + DIR_ENTRY_HEADER_SIZE].copy_from_slice(entry_bytes);
                dir_data[offset + DIR_ENTRY_HEADER_SIZE
                    ..offset + DIR_ENTRY_HEADER_SIZE + name_bytes.len()]
                    .copy_from_slice(name_bytes);

                Self::write_inode_data(state, dir_inode_num, &mut dir_inode, &dir_data)?;
                return Ok(());
            }

            offset += entry.rec_len as usize;
        }

        // Need to allocate a new block for the directory
        let new_size = dir_data.len() + state.block_size;
        dir_data.resize(new_size, 0);

        // Write new entry at the start of the new block
        let new_block_offset = new_size - state.block_size;
        let new_entry = DirEntryRaw {
            inode: inode_num,
            rec_len: state.block_size as u16,
            name_len: name_bytes.len() as u8,
            file_type,
        };

        let entry_bytes = unsafe {
            core::slice::from_raw_parts(
                &new_entry as *const DirEntryRaw as *const u8,
                DIR_ENTRY_HEADER_SIZE,
            )
        };
        dir_data[new_block_offset..new_block_offset + DIR_ENTRY_HEADER_SIZE]
            .copy_from_slice(entry_bytes);
        dir_data[new_block_offset + DIR_ENTRY_HEADER_SIZE
            ..new_block_offset + DIR_ENTRY_HEADER_SIZE + name_bytes.len()]
            .copy_from_slice(name_bytes);

        Self::write_inode_data(state, dir_inode_num, &mut dir_inode, &dir_data)?;
        Ok(())
    }

    fn remove_dir_entry(
        state: &mut Ext2State,
        dir_inode_num: u32,
        name: &str,
    ) -> Result<u32, FsError> {
        let mut dir_inode = Self::read_inode(state, dir_inode_num)?;
        let mut dir_data = Self::read_inode_data(state, &dir_inode)?;

        let mut offset = 0;
        let mut prev_offset: Option<usize> = None;

        while offset + DIR_ENTRY_HEADER_SIZE <= dir_data.len() {
            let entry: DirEntryRaw =
                unsafe { core::ptr::read_unaligned(dir_data[offset..].as_ptr() as *const _) };

            if entry.rec_len == 0 {
                break;
            }

            if entry.inode != 0 {
                let name_start = offset + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + entry.name_len as usize;

                if name_end <= dir_data.len() {
                    if let Ok(entry_name) = core::str::from_utf8(&dir_data[name_start..name_end]) {
                        if entry_name == name {
                            let removed_inode = entry.inode;

                            if let Some(prev) = prev_offset {
                                // Merge with previous entry
                                let prev_entry: DirEntryRaw = unsafe {
                                    core::ptr::read_unaligned(dir_data[prev..].as_ptr() as *const _)
                                };
                                let new_rec_len = prev_entry.rec_len + entry.rec_len;
                                dir_data[prev + 4] = new_rec_len as u8;
                                dir_data[prev + 5] = (new_rec_len >> 8) as u8;
                            } else {
                                // Zero out the inode field
                                dir_data[offset] = 0;
                                dir_data[offset + 1] = 0;
                                dir_data[offset + 2] = 0;
                                dir_data[offset + 3] = 0;
                            }

                            Self::write_inode_data(
                                state,
                                dir_inode_num,
                                &mut dir_inode,
                                &dir_data,
                            )?;
                            return Ok(removed_inode);
                        }
                    }
                }
            }

            if entry.inode != 0 {
                prev_offset = Some(offset);
            }
            offset += entry.rec_len as usize;
        }

        Err(FsError::NotFound)
    }

    // ========================================================================
    // Path Resolution
    // ========================================================================

    fn lookup_path(&self, path: &str) -> Result<u32, FsError> {
        let state = self.state.lock();
        Self::lookup_path_internal(&state, path)
    }

    fn lookup_path_internal(state: &Ext2State, path: &str) -> Result<u32, FsError> {
        let components = path_components(path);

        if components.is_empty() {
            return Ok(ROOT_INODE);
        }

        let mut current_inode = ROOT_INODE;

        for component in components {
            let inode = Self::read_inode(state, current_inode)?;

            if (inode.type_perms & 0xF000) != S_IFDIR {
                return Err(FsError::NotADirectory);
            }

            let dir_data = Self::read_inode_data(state, &inode)?;
            let entries = Self::parse_directory(&dir_data);

            let found = entries.iter().find(|(_, name, _)| name == component);

            match found {
                Some((inode_num, _, _)) => current_inode = *inode_num,
                None => return Err(FsError::NotFound),
            }
        }

        Ok(current_inode)
    }

    fn lookup_parent(&self, path: &str) -> Result<(u32, String), FsError> {
        let (parent_path, name) = split_path(path);
        if name.is_empty() {
            return Err(FsError::InvalidPath);
        }
        let state = self.state.lock();
        let parent_path = if parent_path.is_empty() {
            "/"
        } else {
            parent_path
        };
        let parent_inode = Self::lookup_path_internal(&state, parent_path)?;
        Ok((parent_inode, name.to_string()))
    }
}

// ============================================================================
// Current Time Helper
// ============================================================================

fn current_time() -> u32 {
    crate::timer::utc_time_us()
        .map(|us| (us / 1_000_000) as u32)
        .unwrap_or(0)
}

// ============================================================================
// Filesystem Trait Implementation
// ============================================================================

impl Filesystem for Ext2Filesystem {
    fn name(&self) -> &str {
        "ext2"
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let inode_num = self.lookup_path(path)?;
        let state = self.state.lock();
        let inode = Self::read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) != S_IFDIR {
            return Err(FsError::NotADirectory);
        }

        let dir_data = Self::read_inode_data(&state, &inode)?;
        let raw_entries = Self::parse_directory(&dir_data);

        let entries = raw_entries
            .into_iter()
            .filter(|(inode, name, _)| *inode != 0 && name != "." && name != "..")
            .map(|(inode_num, name, file_type)| {
                let is_dir = file_type == FT_DIR;
                let size = if is_dir {
                    0
                } else {
                    Self::read_inode(&state, inode_num)
                        .map(|i| i.size_lower as u64)
                        .unwrap_or(0)
                };
                DirEntry { name, is_dir, size }
            })
            .collect();

        Ok(entries)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let inode_num = self.lookup_path(path)?;
        let state = self.state.lock();
        let inode = Self::read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) == S_IFDIR {
            return Err(FsError::NotAFile);
        }

        Self::read_inode_data(&state, &inode)
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        // Try to find existing file
        match self.lookup_path(path) {
            Ok(inode_num) => {
                // File exists - truncate and write
                let mut state = self.state.lock();
                let mut inode = Self::read_inode(&state, inode_num)?;

                if (inode.type_perms & 0xF000) == S_IFDIR {
                    return Err(FsError::NotAFile);
                }

                Self::truncate_inode(&mut state, &mut inode)?;
                Self::write_inode_data(&mut state, inode_num, &mut inode, data)?;
                Ok(())
            }
            Err(FsError::NotFound) => {
                // Create new file
                let (parent_inode, name) = self.lookup_parent(path)?;
                let mut state = self.state.lock();

                // Allocate inode
                let inode_num = Self::allocate_inode(&mut state, false)?;

                // Initialize inode
                let now = current_time();
                let mut inode = Inode {
                    type_perms: DEFAULT_FILE_PERMS,
                    uid: 0,
                    size_lower: 0,
                    access_time: now,
                    creation_time: now,
                    modification_time: now,
                    hard_links: 1,
                    ..Default::default()
                };

                // Write data
                Self::write_inode_data(&mut state, inode_num, &mut inode, data)?;

                // Add directory entry
                Self::add_dir_entry(&mut state, parent_inode, &name, inode_num, FT_REG_FILE)?;

                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        match self.lookup_path(path) {
            Ok(inode_num) => {
                let mut state = self.state.lock();
                let mut inode = Self::read_inode(&state, inode_num)?;

                if (inode.type_perms & 0xF000) == S_IFDIR {
                    return Err(FsError::NotAFile);
                }

                // Read existing data
                let mut existing = Self::read_inode_data(&state, &inode)?;
                existing.extend_from_slice(data);

                // Write back
                Self::write_inode_data(&mut state, inode_num, &mut inode, &existing)?;
                Ok(())
            }
            Err(FsError::NotFound) => {
                // Create new file
                self.write_file(path, data)
            }
            Err(e) => Err(e),
        }
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        // Check if already exists
        if self.lookup_path(path).is_ok() {
            return Err(FsError::AlreadyExists);
        }

        let (parent_inode_num, name) = self.lookup_parent(path)?;
        let mut state = self.state.lock();

        // Allocate inode
        let inode_num = Self::allocate_inode(&mut state, true)?;

        // Initialize directory inode
        let now = current_time();
        let mut inode = Inode {
            type_perms: DEFAULT_DIR_PERMS,
            uid: 0,
            size_lower: 0,
            access_time: now,
            creation_time: now,
            modification_time: now,
            hard_links: 2, // . and parent's link
            ..Default::default()
        };

        // Allocate initial block for directory entries
        let block = Self::allocate_block(&mut state)?;
        inode.direct_blocks[0] = block;
        inode.size_lower = state.block_size as u32;
        inode.sectors_used = (state.block_size / 512) as u32;

        // Create . and .. entries
        let mut dir_data = vec![0u8; state.block_size];

        // . entry
        let dot_entry = DirEntryRaw {
            inode: inode_num,
            rec_len: 12,
            name_len: 1,
            file_type: FT_DIR,
        };
        let entry_bytes = unsafe {
            core::slice::from_raw_parts(
                &dot_entry as *const DirEntryRaw as *const u8,
                DIR_ENTRY_HEADER_SIZE,
            )
        };
        dir_data[0..DIR_ENTRY_HEADER_SIZE].copy_from_slice(entry_bytes);
        dir_data[DIR_ENTRY_HEADER_SIZE] = b'.';

        // .. entry
        let dotdot_entry = DirEntryRaw {
            inode: parent_inode_num,
            rec_len: (state.block_size - 12) as u16,
            name_len: 2,
            file_type: FT_DIR,
        };
        let entry_bytes = unsafe {
            core::slice::from_raw_parts(
                &dotdot_entry as *const DirEntryRaw as *const u8,
                DIR_ENTRY_HEADER_SIZE,
            )
        };
        dir_data[12..12 + DIR_ENTRY_HEADER_SIZE].copy_from_slice(entry_bytes);
        dir_data[12 + DIR_ENTRY_HEADER_SIZE] = b'.';
        dir_data[12 + DIR_ENTRY_HEADER_SIZE + 1] = b'.';

        Self::write_block(&state, block, &dir_data)?;
        Self::write_inode(&state, inode_num, &inode)?;

        // Update parent's hard link count
        let mut parent_inode = Self::read_inode(&state, parent_inode_num)?;
        parent_inode.hard_links += 1;
        Self::write_inode(&state, parent_inode_num, &parent_inode)?;

        // Add entry to parent
        Self::add_dir_entry(&mut state, parent_inode_num, &name, inode_num, FT_DIR)?;

        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<(), FsError> {
        let inode_num = self.lookup_path(path)?;
        let (parent_inode, name) = self.lookup_parent(path)?;

        let mut state = self.state.lock();
        let mut inode = Self::read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) == S_IFDIR {
            return Err(FsError::NotAFile);
        }

        // Remove directory entry
        Self::remove_dir_entry(&mut state, parent_inode, &name)?;

        // Decrement hard link count
        inode.hard_links = inode.hard_links.saturating_sub(1);

        if inode.hard_links == 0 {
            // Free all blocks
            Self::truncate_inode(&mut state, &mut inode)?;
            inode.deletion_time = current_time();
            Self::write_inode(&state, inode_num, &inode)?;

            // Free inode
            Self::free_inode(&mut state, inode_num, false)?;
        } else {
            Self::write_inode(&state, inode_num, &inode)?;
        }

        Ok(())
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        let inode_num = self.lookup_path(path)?;

        if inode_num == ROOT_INODE {
            return Err(FsError::PermissionDenied);
        }

        let (parent_inode_num, name) = self.lookup_parent(path)?;

        let mut state = self.state.lock();
        let mut inode = Self::read_inode(&state, inode_num)?;

        if (inode.type_perms & 0xF000) != S_IFDIR {
            return Err(FsError::NotADirectory);
        }

        // Check if directory is empty (only . and ..)
        let dir_data = Self::read_inode_data(&state, &inode)?;
        let entries = Self::parse_directory(&dir_data);
        let non_dot_entries: Vec<_> = entries
            .iter()
            .filter(|(_, n, _)| n != "." && n != "..")
            .collect();

        if !non_dot_entries.is_empty() {
            return Err(FsError::DirectoryNotEmpty);
        }

        // Remove directory entry from parent
        Self::remove_dir_entry(&mut state, parent_inode_num, &name)?;

        // Update parent's hard link count
        let mut parent_inode = Self::read_inode(&state, parent_inode_num)?;
        parent_inode.hard_links = parent_inode.hard_links.saturating_sub(1);
        Self::write_inode(&state, parent_inode_num, &parent_inode)?;

        // Free blocks
        Self::truncate_inode(&mut state, &mut inode)?;
        inode.deletion_time = current_time();
        Self::write_inode(&state, inode_num, &inode)?;

        // Free inode
        Self::free_inode(&mut state, inode_num, true)?;

        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        self.lookup_path(path).is_ok()
    }

    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        let inode_num = self.lookup_path(path)?;
        let state = self.state.lock();
        let inode = Self::read_inode(&state, inode_num)?;

        let is_dir = (inode.type_perms & 0xF000) == S_IFDIR;

        Ok(Metadata {
            is_dir,
            size: inode.size_lower as u64,
            created: Some(inode.creation_time as u64),
            modified: Some(inode.modification_time as u64),
            accessed: Some(inode.access_time as u64),
        })
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        let state = self.state.lock();
        let total_blocks = state.superblock.total_blocks;
        let unallocated_blocks = state.superblock.unallocated_blocks;

        Ok(FsStats {
            block_size: state.block_size as u32,
            total_blocks: total_blocks as u64,
            free_blocks: unallocated_blocks as u64,
        })
    }

    fn sync(&self) -> Result<(), FsError> {
        // All writes are synchronous, nothing to sync
        Ok(())
    }
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
