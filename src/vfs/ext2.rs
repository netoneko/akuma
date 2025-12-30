//! Ext2 Filesystem Implementation
//!
//! A minimal ext2 filesystem driver for no_std environments.
//! Based on the ext2 specification and inspired by the mikros ext2 implementation.
//! Reference: https://gitea.pterpstra.com/mikros/ext2

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use spinning_top::Spinlock;

use super::{path_components, DirEntry, Filesystem, FsError, FsStats, Metadata};
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
const S_IFLNK: u16 = 0xA000; // Symbolic link

/// Directory entry file type constants
const FT_UNKNOWN: u8 = 0;
const FT_REG_FILE: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHRDEV: u8 = 3;
const FT_BLKDEV: u8 = 4;
const FT_FIFO: u8 = 5;
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

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
    block_size_log: u32,       // log2(block_size) - 10
    fragment_size_log: u32,    // log2(fragment_size) - 10
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
    // Extended superblock fields (version >= 1)
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
    // Padding to 1024 bytes
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
    size_upper: u32, // For regular files in revision 1
    fragment_addr: u32,
    os_specific_2: [u8; 12],
}

/// Directory entry (variable size on disk)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct DirEntryRaw {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
    // name follows (name_len bytes, padded to 4-byte boundary in rec_len)
}

// ============================================================================
// Ext2 Filesystem
// ============================================================================

/// Ext2 filesystem state
struct Ext2State {
    superblock: Superblock,
    block_size: usize,
    inodes_per_group: u32,
    inode_size: u16,
    block_group_count: u32,
}

/// Ext2 filesystem implementation
pub struct Ext2Filesystem {
    state: Spinlock<Ext2State>,
}

impl Ext2Filesystem {
    /// Create a new Ext2 filesystem from the block device
    pub fn new() -> Result<Self, FsError> {
        log("[Ext2] Mounting ext2 filesystem...\n");

        // Read superblock
        let mut sb_buf = [0u8; 1024];
        block::read_bytes(SUPERBLOCK_OFFSET, &mut sb_buf)
            .map_err(|_| FsError::IoError)?;

        // SAFETY: Superblock is repr(C, packed) and we're reading raw bytes
        let superblock: Superblock = unsafe { core::ptr::read_unaligned(sb_buf.as_ptr() as *const _) };

        // Copy packed fields to local variables to avoid alignment issues
        let magic = superblock.magic;
        let block_size_log = superblock.block_size_log;
        let version_major = superblock.version_major;
        let sb_inode_size = superblock.inode_size;
        let total_blocks = superblock.total_blocks;
        let total_inodes = superblock.total_inodes;
        let blocks_per_group = superblock.blocks_per_group;

        // Verify magic number
        if magic != EXT2_MAGIC {
            console::print(&alloc::format!(
                "[Ext2] Invalid magic: 0x{:04X} (expected 0x{:04X})\n",
                magic, EXT2_MAGIC
            ));
            return Err(FsError::NoFilesystem);
        }

        let block_size = 1024usize << block_size_log;
        let inode_size = if version_major >= 1 {
            sb_inode_size
        } else {
            128
        };

        let block_group_count = (total_blocks + blocks_per_group - 1) / blocks_per_group;

        console::print(&alloc::format!(
            "[Ext2] Mounted: {} blocks, {} inodes, {} byte blocks, {} block groups\n",
            total_blocks,
            total_inodes,
            block_size,
            block_group_count
        ));

        let state = Ext2State {
            superblock,
            block_size,
            inodes_per_group: superblock.inodes_per_group,
            inode_size,
            block_group_count,
        };

        Ok(Self {
            state: Spinlock::new(state),
        })
    }

    /// Mount ext2 and return a boxed Filesystem trait object
    pub fn mount() -> Result<Box<dyn Filesystem>, FsError> {
        Ok(Box::new(Self::new()?))
    }

    /// Read a block from the device
    fn read_block(state: &Ext2State, block_num: u32) -> Result<Vec<u8>, FsError> {
        let mut buf = vec![0u8; state.block_size];
        let offset = block_num as u64 * state.block_size as u64;
        block::read_bytes(offset, &mut buf).map_err(|_| FsError::IoError)?;
        Ok(buf)
    }

    /// Write a block to the device
    fn write_block(state: &Ext2State, block_num: u32, data: &[u8]) -> Result<(), FsError> {
        if data.len() != state.block_size {
            return Err(FsError::InvalidPath);
        }
        let offset = block_num as u64 * state.block_size as u64;
        block::write_bytes(offset, data).map_err(|_| FsError::IoError)?;
        Ok(())
    }

    /// Read a block group descriptor
    fn read_bgd(state: &Ext2State, group: u32) -> Result<BlockGroupDescriptor, FsError> {
        // BGD table starts at block 1 (for 1K blocks) or block 0 (for larger blocks)
        let bgd_table_block = if state.block_size == 1024 { 2 } else { 1 };
        let bgd_offset = bgd_table_block as u64 * state.block_size as u64
            + group as u64 * size_of::<BlockGroupDescriptor>() as u64;

        let mut buf = [0u8; size_of::<BlockGroupDescriptor>()];
        block::read_bytes(bgd_offset, &mut buf).map_err(|_| FsError::IoError)?;

        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) })
    }

    /// Read an inode
    fn read_inode(state: &Ext2State, inode_num: u32) -> Result<Inode, FsError> {
        if inode_num == 0 {
            return Err(FsError::NotFound);
        }

        // Inode numbers are 1-based
        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let index_in_group = inode_idx % state.inodes_per_group;

        let bgd = Self::read_bgd(state, group)?;

        let inode_offset = bgd.inode_table as u64 * state.block_size as u64
            + index_in_group as u64 * state.inode_size as u64;

        let mut buf = vec![0u8; state.inode_size as usize];
        block::read_bytes(inode_offset, &mut buf).map_err(|_| FsError::IoError)?;

        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) })
    }

    /// Write an inode
    fn write_inode(state: &Ext2State, inode_num: u32, inode: &Inode) -> Result<(), FsError> {
        if inode_num == 0 {
            return Err(FsError::NotFound);
        }

        let inode_idx = inode_num - 1;
        let group = inode_idx / state.inodes_per_group;
        let index_in_group = inode_idx % state.inodes_per_group;

        let bgd = Self::read_bgd(state, group)?;

        let inode_offset = bgd.inode_table as u64 * state.block_size as u64
            + index_in_group as u64 * state.inode_size as u64;

        let buf = unsafe {
            core::slice::from_raw_parts(
                inode as *const Inode as *const u8,
                size_of::<Inode>(),
            )
        };
        block::write_bytes(inode_offset, buf).map_err(|_| FsError::IoError)?;

        Ok(())
    }

    /// Get a block number from an inode given a logical block index
    fn get_block_num(state: &Ext2State, inode: &Inode, logical_block: u32) -> Result<Option<u32>, FsError> {
        let ptrs_per_block = (state.block_size / 4) as u32;

        if logical_block < 12 {
            // Direct block
            let block = inode.direct_blocks[logical_block as usize];
            return Ok(if block == 0 { None } else { Some(block) });
        }

        let logical_block = logical_block - 12;

        if logical_block < ptrs_per_block {
            // Singly indirect
            if inode.indirect_block == 0 {
                return Ok(None);
            }
            let indirect = Self::read_block(state, inode.indirect_block)?;
            let block = u32::from_le_bytes([
                indirect[logical_block as usize * 4],
                indirect[logical_block as usize * 4 + 1],
                indirect[logical_block as usize * 4 + 2],
                indirect[logical_block as usize * 4 + 3],
            ]);
            return Ok(if block == 0 { None } else { Some(block) });
        }

        let logical_block = logical_block - ptrs_per_block;

        if logical_block < ptrs_per_block * ptrs_per_block {
            // Doubly indirect
            if inode.double_indirect_block == 0 {
                return Ok(None);
            }
            let idx1 = logical_block / ptrs_per_block;
            let idx2 = logical_block % ptrs_per_block;

            let double_indirect = Self::read_block(state, inode.double_indirect_block)?;
            let indirect_block = u32::from_le_bytes([
                double_indirect[idx1 as usize * 4],
                double_indirect[idx1 as usize * 4 + 1],
                double_indirect[idx1 as usize * 4 + 2],
                double_indirect[idx1 as usize * 4 + 3],
            ]);

            if indirect_block == 0 {
                return Ok(None);
            }

            let indirect = Self::read_block(state, indirect_block)?;
            let block = u32::from_le_bytes([
                indirect[idx2 as usize * 4],
                indirect[idx2 as usize * 4 + 1],
                indirect[idx2 as usize * 4 + 2],
                indirect[idx2 as usize * 4 + 3],
            ]);
            return Ok(if block == 0 { None } else { Some(block) });
        }

        // Triple indirect (not commonly needed for small files)
        Err(FsError::NotSupported)
    }

    /// Read file data from an inode
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
                // Sparse file - fill with zeros
                let remaining = size - data.len();
                let to_copy = core::cmp::min(remaining, state.block_size);
                data.extend(core::iter::repeat(0).take(to_copy));
            }
        }

        Ok(data)
    }

    /// Parse directory entries from raw directory data
    fn parse_directory(data: &[u8]) -> Vec<(u32, String, u8)> {
        let mut entries = Vec::new();
        let mut offset = 0;

        while offset + 8 <= data.len() {
            let entry: DirEntryRaw = unsafe {
                core::ptr::read_unaligned(data[offset..].as_ptr() as *const _)
            };

            if entry.inode == 0 || entry.rec_len == 0 {
                break;
            }

            let name_start = offset + 8;
            let name_end = name_start + entry.name_len as usize;

            if name_end <= data.len() {
                if let Ok(name) = core::str::from_utf8(&data[name_start..name_end]) {
                    entries.push((entry.inode, name.to_string(), entry.file_type));
                }
            }

            offset += entry.rec_len as usize;
            if offset >= data.len() {
                break;
            }
        }

        entries
    }

    /// Look up an inode by path
    fn lookup_path(&self, path: &str) -> Result<u32, FsError> {
        let state = self.state.lock();
        let components = path_components(path);

        if components.is_empty() {
            return Ok(ROOT_INODE);
        }

        let mut current_inode = ROOT_INODE;

        for component in components {
            let inode = Self::read_inode(&state, current_inode)?;

            // Verify it's a directory
            if (inode.type_perms & 0xF000) != S_IFDIR {
                return Err(FsError::NotADirectory);
            }

            let dir_data = Self::read_inode_data(&state, &inode)?;
            let entries = Self::parse_directory(&dir_data);

            let found = entries.iter().find(|(_, name, _)| name == component);

            match found {
                Some((inode_num, _, _)) => current_inode = *inode_num,
                None => return Err(FsError::NotFound),
            }
        }

        Ok(current_inode)
    }
}

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

    fn write_file(&self, _path: &str, _data: &[u8]) -> Result<(), FsError> {
        // Write support requires block allocation, which is complex
        // For now, return read-only error
        Err(FsError::ReadOnly)
    }

    fn append_file(&self, _path: &str, _data: &[u8]) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn create_dir(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn remove_file(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn remove_dir(&self, _path: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
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

        Ok(FsStats {
            block_size: state.block_size as u32,
            total_blocks: state.superblock.total_blocks as u64,
            free_blocks: state.superblock.unallocated_blocks as u64,
        })
    }

    fn sync(&self) -> Result<(), FsError> {
        Ok(())
    }
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
