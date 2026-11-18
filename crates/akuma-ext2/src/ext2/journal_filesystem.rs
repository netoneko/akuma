//! Serialized, fail-closed operation boundaries for the transaction device.
//! Not yet exposed by the kernel mount path.

use alloc::{boxed::Box, collections::BTreeSet, string::String, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use akuma_locks_rw_cell::RecoverableCell;
use akuma_vfs::{DirEntry, Filesystem, FsError, FsStats, Metadata};
use super::device::{JournaledDevice, JournalStorage};
use super::super::{Ext2Filesystem, Ext2State, MountMode, state_hold_guard};

const NO_OWNER: usize = usize::MAX;

/// One serialized filesystem instance over a transaction device. Any failed
/// mutation poisons the instance, because the existing ext2 implementation
/// cannot yet roll back all in-memory state. The caller must discard it and
/// recover the underlying filesystem before retrying. This conservative
/// wrapper is not connected to the kernel's writable mount path.
pub struct JournaledFilesystem<B: JournalStorage> {
    inner: Ext2Filesystem<JournaledDevice<B>>,
    is_ext4: bool,
    gate: Box<RecoverableCell<()>>,
    owner: AtomicUsize,
}

impl<B: JournalStorage> JournaledFilesystem<B> {
    /// Construct a writable instance for the bounded, clean classic ext3
    /// profile. The caller's storage type must explicitly promise the journal
    /// durability and atomic-sector contract and ensure exclusive access to
    /// the backing device for the mount's lifetime. This is only connected to
    /// a QEMU test mount; errors after a mutation conservatively poison it.
    pub fn mount(dev: B, utc_time_us: fn() -> u64, cache_cap_bytes: Option<usize>) -> Result<Self, FsError> {
        let mut sb = [0u8; 1024];
        dev.read_bytes(1024, &mut sb).map_err(|_| FsError::IoError)?;
        let journal_inode = u32::from_le_bytes(sb[224..228].try_into().map_err(|_| FsError::Corrupt)?);
        let fs = Ext2Filesystem::new_with_options(dev, utc_time_us, cache_cap_bytes, MountMode::ReadOnly)?;
        let (mapping, protected, inode_range, block_size, total, uuid) = {
            let state = fs.read_state();
            let allowed_incompat = 2 | if cfg!(feature = "ext4") { 0x40 } else { 0 };
            if state.superblock.feature_compat & 4 == 0
                || state.superblock.feature_incompat & !allowed_incompat != 0
                || state.superblock.feature_ro_compat & !(1 | 2) != 0
                || journal_inode == 0 || journal_inode > state.superblock.total_inodes {
                return Err(FsError::NotSupported);
            }
            let inode = fs.read_inode(&state, journal_inode)?;
            if inode.file_size() % state.block_size as u64 != 0 || inode.file_size() > 64 * 1024 * 1024 {
                return Err(FsError::NotSupported);
            }
            let mut protected = BTreeSet::new();
            fn protect<B: JournalStorage>(fs: &Ext2Filesystem<B>, state: &Ext2State, block: u32, depth: u8, protected: &mut BTreeSet<u32>) -> Result<(), FsError> {
                if block == 0 { return Ok(()); }
                if block >= state.superblock.total_blocks || !protected.insert(block) || protected.len() > 65536 {
                    return Err(FsError::Corrupt);
                }
                if depth > 1 {
                    let bytes = fs.read_block(state, block)?;
                    for entry in bytes.chunks_exact(4) {
                        let child = u32::from_le_bytes(entry.try_into().map_err(|_| FsError::Corrupt)?);
                        if child != 0 { protect(fs, state, child, depth - 1, protected)?; }
                    }
                }
                Ok(())
            }
            if inode.flags & 0x80000 != 0 {
                // The initial ext4 profile accepts a journal whose extents all
                // fit in the inode. External extent nodes need protection too.
                let mut raw = [0u8; 128];
                inode.serialize(&mut raw);
                if u16::from_le_bytes([raw[46], raw[47]]) != 0 { return Err(FsError::NotSupported); }
            } else {
                protect(&fs, &state, inode.indirect_block, 1, &mut protected)?;
                protect(&fs, &state, inode.double_indirect_block, 2, &mut protected)?;
                protect(&fs, &state, inode.triple_indirect_block, 3, &mut protected)?;
            }
            let mut mapping = Vec::new();
            for logical in 0..inode.file_size() / state.block_size as u64 {
                mapping.push(fs.get_block_num(&state, &inode, logical as u32)?.ok_or(FsError::Corrupt)?);
            }
            let index = journal_inode - 1;
            let group = index / state.inodes_per_group;
            let within = index % state.inodes_per_group;
            let descriptor = fs.read_bgd(&state, group)?;
            let start = u64::from(descriptor.inode_table) * state.block_size as u64
                + u64::from(within) * u64::from(state.inode_size);
            (mapping, protected, start..start + u64::from(state.inode_size), state.block_size,
                state.superblock.total_blocks, state.superblock.uuid)
        };
        let dev = JournaledDevice::new(fs.dev, block_size, total, mapping, protected, inode_range, uuid)?;
        let inner = Ext2Filesystem::new_internal(dev, utc_time_us, cache_cap_bytes, MountMode::ReadWrite, false, true)?;
        Self::new(inner)
    }

    pub fn new(inner: Ext2Filesystem<JournaledDevice<B>>) -> Result<Self, FsError> {
        if inner.mode != MountMode::ReadWrite || !inner.journal_writes || inner.dev.is_poisoned() { return Err(FsError::NotSupported); }
        let is_ext4 = inner.read_state().superblock.feature_incompat & 0x40 != 0;
        Ok(Self { inner, is_ext4, gate: Box::new(RecoverableCell::new(())), owner: AtomicUsize::new(NO_OWNER) })
    }

    fn locked<T>(&self, f: impl FnOnce(&Ext2Filesystem<JournaledDevice<B>>) -> Result<T, FsError>) -> Result<T, FsError> {
        if self.inner.dev.is_poisoned() { return Err(FsError::IoError); }
        let (_guard, _hold) = self.gate.write_holding(state_hold_guard);
        if self.inner.dev.is_poisoned() { return Err(FsError::IoError); }
        self.owner.store(akuma_primitives::preempt::current_tid(), Ordering::Release);
        let result = f(&self.inner);
        self.owner.store(NO_OWNER, Ordering::Release);
        result
    }

    fn mutate<T>(&self, f: impl FnOnce(&Ext2Filesystem<JournaledDevice<B>>) -> Result<T, FsError>) -> Result<T, FsError> {
        self.locked(|fs| {
            let before = fs.write_epoch.load(Ordering::Relaxed);
            fs.dev.begin()?;
            let result = f(fs).and_then(|value| { fs.sync()?; Ok(value) });
            if result.is_err() && (fs.write_epoch.load(Ordering::Relaxed) != before || !fs.dev.cancel_empty()) {
                fs.dev.abort();
            }
            result
        })
    }

    #[cfg(test)]
    pub(crate) fn simulate_dead_gate_holder(&self, tid: usize) {
        let guard = self.gate.write_as(tid);
        self.owner.store(tid, Ordering::Release);
        core::mem::forget(guard);
    }
}

impl<B: JournalStorage> Filesystem for JournaledFilesystem<B> {
    fn name(&self) -> &str { if self.is_ext4 { "ext4" } else { "ext3" } }
    fn identity(&self) -> u32 { self.inner.identity() }
    fn needs_tid_reap(&self) -> bool { true }
    fn abandon_tid(&self, tid: usize) -> bool {
        let owned = self.owner.load(Ordering::Acquire) == tid;
        if owned {
            // Poison before releasing any lock: a waiter must never observe
            // the interrupted transaction's cache or in-memory metadata.
            self.inner.dev.abandon_transaction(tid);
            self.inner.abandon_tid(tid);
        }
        self.gate.abandon_tid(tid) || owned
    }
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> { self.locked(|fs| fs.read_dir(path)) }
    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> { self.locked(|fs| fs.read_file(path)) }
    fn read_at(&self, path: &str, offset: usize, out: &mut [u8]) -> Result<usize, FsError> { self.locked(|fs| fs.read_at(path, offset, out)) }
    fn read_symlink(&self, path: &str) -> Result<String, FsError> { self.locked(|fs| fs.read_symlink(path)) }
    fn is_symlink(&self, path: &str) -> bool { self.locked(|fs| Ok(fs.is_symlink(path))).unwrap_or(false) }
    fn exists(&self, path: &str) -> bool { self.locked(|fs| Ok(fs.exists(path))).unwrap_or(false) }
    fn metadata(&self, path: &str) -> Result<Metadata, FsError> { self.locked(|fs| fs.metadata(path)) }
    fn metadata_by_inode(&self, inode: u32) -> Result<Metadata, FsError> { self.locked(|fs| fs.metadata_by_inode(inode)) }
    fn resolve_inode(&self, path: &str) -> Result<u32, FsError> { self.locked(|fs| fs.resolve_inode(path)) }
    fn read_at_by_inode(&self, inode: u32, offset: usize, out: &mut [u8]) -> Result<usize, FsError> { self.locked(|fs| fs.read_at_by_inode(inode, offset, out)) }
    // stats may drain deferred frees, and sync may flush them, so both need a transaction.
    fn stats(&self) -> Result<FsStats, FsError> { self.mutate(|fs| fs.stats()) }
    fn sync(&self) -> Result<(), FsError> { self.mutate(|_| Ok(())) }
    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> { self.mutate(|fs| fs.write_file(path, data)) }
    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> { self.mutate(|fs| fs.write_at(path, offset, data)) }
    fn append(&self, path: &str, data: &[u8]) -> Result<(usize, usize), FsError> { self.mutate(|fs| fs.append(path, data)) }
    fn write_at_by_inode(&self, inode: u32, offset: usize, data: &[u8]) -> Result<usize, FsError> { self.mutate(|fs| fs.write_at_by_inode(inode, offset, data)) }
    fn create_dir(&self, path: &str) -> Result<(), FsError> { self.mutate(|fs| fs.create_dir(path)) }
    fn remove_file(&self, path: &str) -> Result<(), FsError> { self.mutate(|fs| fs.remove_file(path)) }
    fn remove_dir(&self, path: &str) -> Result<(), FsError> { self.mutate(|fs| fs.remove_dir(path)) }
    fn create_symlink(&self, path: &str, target: &str) -> Result<(), FsError> { self.mutate(|fs| fs.create_symlink(path, target)) }
    fn create_socket_node(&self, path: &str) -> Result<akuma_primitives::InodePin, FsError> {
        self.mutate(|fs| fs.create_socket_node(path))
    }
    fn lookup_socket_node(&self, path: &str) -> Result<akuma_primitives::InodePin, FsError> {
        self.inner.lookup_socket_node(path)
    }
    fn rename(&self, old: &str, new: &str) -> Result<(), FsError> { self.mutate(|fs| fs.rename(old, new)) }
    fn link(&self, old: &str, new: &str) -> Result<(), FsError> { self.mutate(|fs| fs.link(old, new)) }
    fn chmod(&self, path: &str, mode: u32) -> Result<(), FsError> { self.mutate(|fs| fs.chmod(path, mode)) }
    fn chown(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), FsError> { self.mutate(|fs| fs.chown(path, uid, gid)) }
    fn set_times(&self, path: &str, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FsError> { self.mutate(|fs| fs.set_times(path, atime, mtime)) }
    fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> { self.mutate(|fs| fs.truncate(path, len)) }
    fn fallocate(&self, path: &str, mode: i32, offset: u64, len: u64) -> Result<(), FsError> { self.mutate(|fs| fs.fallocate(path, mode, offset, len)) }
    fn truncate_by_inode(&self, inode: u32, len: u64) -> Result<(), FsError> { self.mutate(|fs| fs.truncate_by_inode(inode, len)) }
    fn fallocate_by_inode(&self, inode: u32, mode: i32, offset: u64, len: u64) -> Result<(), FsError> { self.mutate(|fs| fs.fallocate_by_inode(inode, mode, offset, len)) }
}
