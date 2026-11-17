//! Explicit transaction-backed block device. This is a building block, not a
//! writable filesystem mount: callers must bracket every mutation and stop
//! using their filesystem instance if a transaction is aborted or fails.

use alloc::{collections::BTreeSet, vec, vec::Vec};
use spinning_top::Spinlock;
use akuma_vfs::FsError;
use crate::BlockDevice;
use super::{staging::WriteSet, writer::{self, JournalIo}};

/// A block device whose flush is a real stable-storage barrier and whose writes
/// are atomic at the stated power-of-two byte granularity. Implementing this is
/// a storage contract, not something inferred from successful write completion.
/// For example, a generic virtio device does not advertise this guarantee.
pub trait JournalStorage: BlockDevice {
    fn atomic_write_bytes(&self) -> usize;
}

struct State {
    active: Option<WriteSet>,
    poisoned: bool,
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
    uuid: [u8; 16],
    max_blocks: usize,
    state: Spinlock<State>,
}

impl<B: JournalStorage> JournaledDevice<B> {
    /// Create an idle transaction device over a previously validated clean
    /// internal journal. `protected` must include journal mapping metadata as
    /// well as every journal data block; all mappings are filesystem-relative.
    pub fn new(dev: B, block_size: usize, filesystem_blocks: u32, journal: Vec<u32>, mut protected: BTreeSet<u32>, uuid: [u8; 16]) -> Result<Self, FsError> {
        let atomic = dev.atomic_write_bytes();
        if !(1024..=65536).contains(&block_size) || !block_size.is_power_of_two()
            || atomic < 512 || !atomic.is_power_of_two() || atomic > block_size
            || journal.len() < 4 || filesystem_blocks == 0 {
            return Err(FsError::NotSupported);
        }
        let mut unique = BTreeSet::new();
        for &block in &journal {
            if block >= filesystem_blocks || !unique.insert(block) { return Err(FsError::Corrupt); }
            protected.insert(block);
        }
        if protected.iter().any(|&block| block >= filesystem_blocks) { return Err(FsError::Corrupt); }
        if protected.contains(&((1024 / block_size) as u32)) { return Err(FsError::Corrupt); }
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
        Ok(Self { dev, block_size, filesystem_blocks, journal, protected, uuid, max_blocks,
            state: Spinlock::new(State { active: None, poisoned: false }) })
    }

    pub fn begin(&self) -> Result<(), FsError> {
        let mut state = self.state.lock();
        if state.poisoned { return Err(FsError::IoError); }
        if state.active.is_some() { return Err(FsError::Internal); }
        state.active = Some(WriteSet::new(self.block_size, self.filesystem_blocks, self.max_blocks)?);
        Ok(())
    }

    /// Abort and permanently poison the device. Its filesystem's in-memory
    /// state may already have changed and must not be reused.
    pub fn abort(&self) {
        let mut state = self.state.lock();
        state.active = None;
        state.poisoned = true;
    }

    pub fn is_poisoned(&self) -> bool { self.state.lock().poisoned }

    /// Flush a staged transaction through the journal and checkpoint it. A
    /// failure poisons the device; the result may already be durable and must
    /// be recovered before the backing filesystem is used again.
    pub fn commit(&self) -> Result<(), FsError> {
        let mut state = self.state.lock();
        if state.poisoned { return Err(FsError::IoError); }
        let staged = state.active.take().ok_or(FsError::Internal)?.into_blocks();
        if staged.is_empty() { return Ok(()); }
        let mut raw = RawJournal { dev: &self.dev, block_size: self.block_size, journal: &self.journal, protected: &self.protected, uuid: &self.uuid };
        let result = writer::commit(&mut raw, self.block_size, self.journal.len() as u64,
            self.filesystem_blocks, (1024 / self.block_size) as u32, &self.uuid, &staged);
        if result.is_err() { state.poisoned = true; }
        result
    }
}

impl<B: JournalStorage> BlockDevice for JournaledDevice<B> {
    fn read_bytes(&self, offset: u64, out: &mut [u8]) -> Result<(), ()> {
        let state = self.state.lock();
        if state.poisoned { return Err(()); }
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
        let mut state = self.state.lock();
        if state.poisoned { return Err(()); }
        if state.active.is_none() { state.poisoned = true; return Err(()); }
        let result = state.active.as_mut().expect("checked above").stage(offset, data, |block| {
            let mut bytes = vec![0; self.block_size];
            self.dev.read_bytes(u64::from(block) * self.block_size as u64, &mut bytes).map_err(|_| FsError::IoError)?;
            Ok(bytes)
        });
        if result.is_err() { state.active = None; state.poisoned = true; }
        result.map_err(|_| ())
    }
    fn flush(&self) -> Result<(), ()> {
        if self.state.lock().active.is_some() { self.commit().map_err(|_| ()) }
        else if self.is_poisoned() { Err(()) }
        else { self.dev.flush() }
    }
}

struct RawJournal<'a, B> {
    dev: &'a B,
    block_size: usize,
    journal: &'a [u32],
    protected: &'a BTreeSet<u32>,
    uuid: &'a [u8; 16],
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
        self.dev.write_bytes(u64::from(physical) * self.block_size as u64, data).map_err(|_| FsError::IoError)
    }
    fn write_home(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
        self.dev.write_bytes(u64::from(block) * self.block_size as u64, data).map_err(|_| FsError::IoError)
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
        self.dev.write_bytes((block * self.block_size) as u64, &data).map_err(|_| FsError::IoError)
    }
    fn flush(&mut self) -> Result<(), FsError> { self.dev.flush().map_err(|_| FsError::IoError) }
    fn home_is_allowed(&self, block: u32) -> bool { !self.protected.contains(&block) }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let device = JournaledDevice::new(disk.clone(), 1024, 16, (2..10).collect(), BTreeSet::new(), uuid).unwrap();
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
}
