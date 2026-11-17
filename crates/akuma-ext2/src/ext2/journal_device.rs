//! Explicit transaction-backed block device. This is a building block, not a
//! writable filesystem mount: callers must bracket every mutation and stop
//! using their filesystem instance if a transaction is aborted or fails.

use alloc::{boxed::Box, collections::BTreeSet, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use core::ops::Range;
use akuma_locks_rw_cell::RecoverableCell;
use akuma_vfs::FsError;
use crate::BlockDevice;
use super::{staging::WriteSet, writer::{self, JournalIo}};

/// A block device whose flush is a real stable-storage barrier and whose writes
/// are atomic at the stated power-of-two byte granularity. Implementing this is
/// a storage contract, not something inferred from successful write completion.
/// For example, a generic virtio device does not advertise this guarantee.
pub trait JournalStorage: BlockDevice {
    fn atomic_write_bytes(&self) -> usize;
    /// Test-only hook after a completed journal write or durability barrier.
    #[cfg(feature = "ext3-qemu-crash-test")]
    fn after_journal_io(&self, _commit_durable: bool) {}
}

struct State {
    active: Option<WriteSet>,
}

/// A device that stages writes until an explicit journal commit.
///
/// A filesystem must serialize complete operations, call `begin` before any
/// mutation, flush its cache into this device, and call `commit` on success.
/// On error or killed-thread recovery it must call `abort` and make the entire
/// filesystem instance unavailable, including cached reads. This type alone
/// cannot roll back the filesystem's in-memory state or detect a killed caller.
pub struct JournaledDevice<B: JournalStorage> {
    dev: B,
    block_size: usize,
    filesystem_blocks: u32,
    journal: Vec<u32>,
    protected: BTreeSet<u32>,
    journal_inode: Range<u64>,
    uuid: [u8; 16],
    max_blocks: usize,
    state: Box<RecoverableCell<State>>,
    poisoned: AtomicBool,
}

impl<B: JournalStorage> JournaledDevice<B> {
    /// Create an idle transaction device over a previously validated clean
    /// internal journal. `protected` must include journal mapping metadata as
    /// well as every journal data block; all mappings are filesystem-relative.
    pub fn new(dev: B, block_size: usize, filesystem_blocks: u32, journal: Vec<u32>, mut protected: BTreeSet<u32>, journal_inode: Range<u64>, uuid: [u8; 16]) -> Result<Self, FsError> {
        let atomic = dev.atomic_write_bytes();
        if !(1024..=65536).contains(&block_size) || !block_size.is_power_of_two()
            || atomic < 512 || !atomic.is_power_of_two() || atomic > block_size
            || journal.len() < 4 || filesystem_blocks == 0 {
            return Err(FsError::NotSupported);
        }
        let mut unique = BTreeSet::new();
        for &block in &journal {
            if block >= filesystem_blocks || protected.contains(&block) || !unique.insert(block) { return Err(FsError::Corrupt); }
            protected.insert(block);
        }
        if protected.iter().any(|&block| block >= filesystem_blocks) { return Err(FsError::Corrupt); }
        if protected.contains(&((1024 / block_size) as u32)) { return Err(FsError::Corrupt); }
        if journal_inode.start >= journal_inode.end || journal_inode.end > u64::from(filesystem_blocks) * block_size as u64 {
            return Err(FsError::Corrupt);
        }
        let first_inode_block = (journal_inode.start / block_size as u64) as u32;
        let last_inode_block = ((journal_inode.end - 1) / block_size as u64) as u32;
        if (first_inode_block..=last_inode_block).any(|block| protected.contains(&block) || block == (1024 / block_size) as u32) {
            return Err(FsError::Corrupt);
        }
        let mut js = vec![0; block_size];
        dev.read_bytes(u64::from(journal[0]) * block_size as u64, &mut js).map_err(|_| FsError::IoError)?;
        super::validate_clean(&js, block_size, journal.len() as u64, &uuid)?;
        let field = |at: usize| u32::from_be_bytes(js[at..at + 4].try_into().expect("validated journal block size"));
        if field(4) != 4 || field(36) != 0 || field(40) & !1 != 0 || field(44) != 0
            || u64::from(field(16)) * block_size as u64 > 64 * 1024 * 1024 {
            return Err(FsError::NotSupported);
        }
        let max = u32::from_be_bytes(js[16..20].try_into().map_err(|_| FsError::Corrupt)?) as usize;
        let first = u32::from_be_bytes(js[20..24].try_into().map_err(|_| FsError::Corrupt)?) as usize;
        let capacity = max.checked_sub(first + 2).ok_or(FsError::Corrupt)?;
        let descriptor_capacity = (block_size - 12 - 16) / 8;
        let max_blocks = capacity.min(descriptor_capacity);
        if max_blocks == 0 { return Err(FsError::NotSupported); }
        // Fail the mount before accepting mutations if the promised barrier
        // is not actually available on this device.
        dev.flush().map_err(|_| FsError::IoError)?;
        Ok(Self { dev, block_size, filesystem_blocks, journal, protected, journal_inode, uuid, max_blocks,
            state: Box::new(RecoverableCell::new(State { active: None })), poisoned: AtomicBool::new(false) })
    }

    pub fn begin(&self) -> Result<(), FsError> {
        if self.is_poisoned() { return Err(FsError::IoError); }
        let (mut state, _hold) = self.state.write_holding(super::super::state_hold_guard);
        if self.is_poisoned() { return Err(FsError::IoError); }
        if state.active.is_some() { return Err(FsError::Internal); }
        state.active = Some(WriteSet::new(self.block_size, self.filesystem_blocks, self.max_blocks)?);
        Ok(())
    }

    /// Cancel an empty transaction. Only safe when the filesystem has also
    /// proved that its own state and cache were not changed.
    pub fn cancel_empty(&self) -> bool {
        if self.is_poisoned() { return false; }
        let (mut state, _hold) = self.state.write_holding(super::super::state_hold_guard);
        if self.is_poisoned() { return false; }
        if state.active.as_ref().is_some_and(WriteSet::is_empty) {
            state.active = None;
            true
        } else { false }
    }

    /// Abort and permanently poison the device. Its filesystem's in-memory
    /// state may already have changed and must not be reused.
    pub fn abort(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    pub fn is_poisoned(&self) -> bool { self.poisoned.load(Ordering::Acquire) }

    /// Poison first, then abandon a dead caller's state lock. The owner must
    /// establish that `tid` died in this transaction before calling this.
    pub fn abandon_transaction(&self, tid: usize) -> bool {
        self.poisoned.store(true, Ordering::Release);
        self.state.abandon_tid(tid)
    }

    /// Flush a staged transaction through the journal and checkpoint it. A
    /// failure poisons the device; the result may already be durable and must
    /// be recovered before the backing filesystem is used again.
    pub fn commit(&self) -> Result<(), FsError> {
        if self.is_poisoned() { return Err(FsError::IoError); }
        let (mut state, _hold) = self.state.write_holding(super::super::state_hold_guard);
        if self.is_poisoned() { return Err(FsError::IoError); }
        let staged = state.active.take().ok_or(FsError::Internal)?.into_blocks();
        if staged.is_empty() { return Ok(()); }
        let mut raw = RawJournal { dev: &self.dev, block_size: self.block_size, journal: &self.journal, protected: &self.protected, uuid: &self.uuid,
            #[cfg(feature = "ext3-qemu-crash-test")]
            commit_pending: false,
        };
        let result = writer::commit(&mut raw, self.block_size, self.journal.len() as u64,
            self.filesystem_blocks, (1024 / self.block_size) as u32, &self.uuid, &staged);
        if result.is_err() { self.poisoned.store(true, Ordering::Release); }
        result
    }
}

impl<B: JournalStorage> BlockDevice for JournaledDevice<B> {
    fn is_failed(&self) -> bool { self.is_poisoned() }
    fn read_bytes(&self, offset: u64, out: &mut [u8]) -> Result<(), ()> {
        if self.is_poisoned() { return Err(()); }
        let (state, _hold) = self.state.read_holding(super::super::state_hold_guard);
        if self.is_poisoned() { return Err(()); }
        self.dev.read_bytes(offset, out)?;
        let end = offset.checked_add(out.len() as u64).ok_or(())?;
        if out.is_empty() { return Ok(()); }
        let bs = self.block_size as u64;
        let first = u32::try_from(offset / bs).map_err(|_| ())?;
        let last = u32::try_from((end - 1) / bs).map_err(|_| ())?;
        if let Some(staged) = &state.active {
            for block in first..=last {
                if let Some(data) = staged.get(block) {
                    let base = u64::from(block) * bs;
                    let lo = offset.max(base); let hi = end.min(base + bs);
                    out[(lo - offset) as usize..(hi - offset) as usize]
                        .copy_from_slice(&data[(lo - base) as usize..(hi - base) as usize]);
                }
            }
        }
        Ok(())
    }
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        if self.is_poisoned() { return Err(()); }
        let (mut state, _hold) = self.state.write_holding(super::super::state_hold_guard);
        if self.is_poisoned() { return Err(()); }
        if state.active.is_none() { self.poisoned.store(true, Ordering::Release); return Err(()); }
        let end = match offset.checked_add(data.len() as u64) {
            Some(end) => end,
            None => { self.poisoned.store(true, Ordering::Release); return Err(()); }
        };
        let lo = offset.max(self.journal_inode.start);
        let hi = end.min(self.journal_inode.end);
        if lo < hi {
            let mut original = vec![0; (hi - lo) as usize];
            if self.dev.read_bytes(lo, &mut original).is_err()
                || original != data[(lo - offset) as usize..(hi - offset) as usize] {
                self.poisoned.store(true, Ordering::Release);
                return Err(());
            }
        }
        let result = state.active.as_mut().expect("checked above").stage(offset, data, |block| {
            let mut bytes = vec![0; self.block_size];
            self.dev.read_bytes(u64::from(block) * self.block_size as u64, &mut bytes).map_err(|_| FsError::IoError)?;
            Ok(bytes)
        });
        if result.is_err() { state.active = None; self.poisoned.store(true, Ordering::Release); }
        result.map_err(|_| ())
    }
    fn flush(&self) -> Result<(), ()> {
        if self.is_poisoned() { return Err(()); }
        let active = {
            let (state, _hold) = self.state.read_holding(super::super::state_hold_guard);
            state.active.is_some()
        };
        if active { self.commit().map_err(|_| ()) } else { self.dev.flush() }
    }
}

struct RawJournal<'a, B> {
    dev: &'a B,
    block_size: usize,
    journal: &'a [u32],
    protected: &'a BTreeSet<u32>,
    uuid: &'a [u8; 16],
    #[cfg(feature = "ext3-qemu-crash-test")]
    commit_pending: bool,
}
impl<B: JournalStorage> JournalIo for RawJournal<'_, B> {
    fn read_journal(&mut self, block: u32) -> Result<Vec<u8>, FsError> {
        let physical = *self.journal.get(block as usize).ok_or(FsError::Corrupt)?;
        let mut data = vec![0; self.block_size];
        self.dev.read_bytes(u64::from(physical) * self.block_size as u64, &mut data).map_err(|_| FsError::IoError)?;
        Ok(data)
    }
    fn write_journal(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
        let physical = *self.journal.get(block as usize).ok_or(FsError::Corrupt)?;
        self.dev.write_bytes(u64::from(physical) * self.block_size as u64, data).map_err(|_| FsError::IoError)?;
        #[cfg(feature = "ext3-qemu-crash-test")]
        {
            self.commit_pending = data.get(..8) == Some(&[0xc0, 0x3b, 0x39, 0x98, 0, 0, 0, 2]);
            self.dev.after_journal_io(false);
        }
        Ok(())
    }
    fn write_home(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
        self.dev.write_bytes(u64::from(block) * self.block_size as u64, data).map_err(|_| FsError::IoError)?;
        #[cfg(feature = "ext3-qemu-crash-test")]
        self.dev.after_journal_io(false);
        Ok(())
    }
    fn set_recovery(&mut self, required: bool) -> Result<(), FsError> {
        let block = 1024 / self.block_size;
        let offset = 1024 % self.block_size;
        let mut data = vec![0; self.block_size];
        self.dev.read_bytes((block * self.block_size) as u64, &mut data).map_err(|_| FsError::IoError)?;
        let sb = data.get_mut(offset..offset + 1024).ok_or(FsError::Corrupt)?;
        if sb.get(56..58) != Some(&[0x53, 0xef]) || sb.get(104..120) != Some(self.uuid.as_slice()) { return Err(FsError::Corrupt); }
        let flags = u32::from_le_bytes(sb[96..100].try_into().map_err(|_| FsError::Corrupt)?);
        sb[96..100].copy_from_slice(&(if required { flags | 4 } else { flags & !4 }).to_le_bytes());
        self.dev.write_bytes((block * self.block_size) as u64, &data).map_err(|_| FsError::IoError)?;
        #[cfg(feature = "ext3-qemu-crash-test")]
        self.dev.after_journal_io(false);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), FsError> {
        self.dev.flush().map_err(|_| FsError::IoError)?;
        #[cfg(feature = "ext3-qemu-crash-test")]
        {
            let committed = self.commit_pending;
            self.commit_pending = false;
            self.dev.after_journal_io(committed);
        }
        Ok(())
    }
    fn home_is_allowed(&self, block: u32) -> bool { !self.protected.contains(&block) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spinning_top::Spinlock;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};
    #[derive(Clone)]
    struct Disk { bytes: Arc<Spinlock<Vec<u8>>>, fail_flush: Arc<AtomicBool> }
    impl BlockDevice for Disk {
        fn read_bytes(&self, offset: u64, out: &mut [u8]) -> Result<(), ()> {
            let data = self.bytes.lock(); let start = usize::try_from(offset).map_err(|_| ())?;
            let end = start.checked_add(out.len()).ok_or(())?;
            out.copy_from_slice(data.get(start..end).ok_or(())?); Ok(())
        }
        fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
            let mut bytes = self.bytes.lock(); let start = usize::try_from(offset).map_err(|_| ())?;
            let end = start.checked_add(data.len()).ok_or(())?;
            bytes.get_mut(start..end).ok_or(())?.copy_from_slice(data); Ok(())
        }
        fn flush(&self) -> Result<(), ()> { if self.fail_flush.load(Ordering::Relaxed) { Err(()) } else { Ok(()) } }
    }
    impl JournalStorage for Disk { fn atomic_write_bytes(&self) -> usize { 512 } }
    fn device() -> (JournaledDevice<Disk>, Disk) {
        let mut bytes = vec![0; 16 * 1024];
        bytes[1024 + 56..1024 + 58].copy_from_slice(&[0x53, 0xef]);
        let uuid = [0x42; 16]; bytes[1024 + 104..1024 + 120].copy_from_slice(&uuid);
        let js = &mut bytes[2 * 1024..3 * 1024];
        js[0..4].copy_from_slice(&0xc03b_3998u32.to_be_bytes());
        js[4..8].copy_from_slice(&4u32.to_be_bytes());
        js[12..16].copy_from_slice(&1024u32.to_be_bytes());
        js[16..20].copy_from_slice(&8u32.to_be_bytes());
        js[20..24].copy_from_slice(&1u32.to_be_bytes());
        js[24..28].copy_from_slice(&1u32.to_be_bytes());
        js[48..64].copy_from_slice(&uuid);
        let disk = Disk { bytes: Arc::new(Spinlock::new(bytes)), fail_flush: Arc::new(AtomicBool::new(false)) };
        let device = JournaledDevice::new(disk.clone(), 1024, 16, (2..10).collect(), BTreeSet::new(), 15 * 1024..15 * 1024 + 128, uuid).unwrap();
        (device, disk)
    }
    #[test]
    fn failed_durability_barrier_poisons_the_device() {
        let (dev, disk) = device();
        dev.begin().unwrap(); dev.write_bytes(12 * 1024, b"changed").unwrap();
        disk.fail_flush.store(true, Ordering::Relaxed);
        assert_eq!(dev.commit(), Err(FsError::IoError));
        assert!(dev.is_poisoned());
        assert_eq!(dev.begin(), Err(FsError::IoError));
        assert!(dev.read_bytes(0, &mut [0]).is_err());
        assert_eq!(&disk.bytes.lock()[12 * 1024..12 * 1024 + 7], &[0; 7]);
        assert_ne!(disk.bytes.lock()[1024 + 96] & 4, 0);
    }
    #[test]
    fn mount_rejects_a_device_that_cannot_flush() {
        let (dev, disk) = device();
        drop(dev);
        disk.fail_flush.store(true, Ordering::Relaxed);
        let result = JournaledDevice::new(disk, 1024, 16, (2..10).collect(), BTreeSet::new(), 15 * 1024..15 * 1024 + 128, [0x42; 16]);
        assert!(matches!(result, Err(FsError::IoError)));
    }
    #[test]
    fn writes_without_a_transaction_poison_the_device() {
        let (dev, _) = device();
        assert!(dev.write_bytes(12 * 1024, b"x").is_err());
        assert!(dev.is_poisoned());
        assert_eq!(dev.begin(), Err(FsError::IoError));
    }
    #[test]
    fn attempts_to_checkpoint_the_journal_are_rejected_before_disk_writes() {
        let (dev, disk) = device();
        let before = disk.bytes.lock().clone();
        dev.begin().unwrap();
        dev.write_bytes(2 * 1024, b"x").unwrap();
        assert_eq!(dev.commit(), Err(FsError::NotSupported));
        assert!(dev.is_poisoned());
        assert_eq!(*disk.bytes.lock(), before);
    }
    #[test]
    fn journal_inode_bytes_cannot_be_changed_by_a_staged_block_write() {
        let (dev, disk) = device();
        let before = disk.bytes.lock().clone();
        dev.begin().unwrap();
        assert!(dev.write_bytes(15 * 1024, &[1; 1024]).is_err());
        assert!(dev.is_poisoned());
        assert_eq!(*disk.bytes.lock(), before);
    }
    #[test]
    fn dead_transaction_holder_is_poisoned_before_its_lock_is_released() {
        let (dev, _) = device();
        let hold = dev.state.write_as(7);
        core::mem::forget(hold);
        assert!(dev.abandon_transaction(7));
        assert!(dev.is_poisoned());
        assert!(dev.state.try_write_as(0).is_some());
        assert_eq!(dev.begin(), Err(FsError::IoError));
    }
}
