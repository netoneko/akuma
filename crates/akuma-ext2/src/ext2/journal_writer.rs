//! Bounded classic JBD transaction writer. This module is not yet wired into
//! filesystem mutations. Records are encoded from the published JBD2 on-disk
//! format: https://docs.kernel.org/filesystems/ext4/journal.html

use alloc::{borrow::Cow, collections::BTreeSet, vec, vec::Vec};
use akuma_vfs::FsError;
use super::{self as journal, ReplayBlock};

const MAGIC: u32 = 0xc03b_3998;

/// Exclusive access to a filesystem and its internal journal during one
/// transaction. All methods operate on complete filesystem blocks. `flush`
/// must make all acknowledged earlier writes durable and ordered. Implementors
/// must guarantee atomic persistence of the sector containing the filesystem
/// recovery bit and journal superblock fields. On any error the caller must
/// stop issuing writes and require recovery before using the filesystem again.
///
/// The filesystem recovery marker must be updated without changing other
/// fields. Journal block numbers are logical offsets into the journal inode;
/// home block numbers are physical filesystem blocks. Implementors must prevent
/// a home block from naming any journal block or its mapping metadata.
pub trait JournalIo {
    fn read_journal(&mut self, block: u32) -> Result<Vec<u8>, FsError>;
    fn write_journal(&mut self, block: u32, data: &[u8]) -> Result<(), FsError>;
    fn write_home(&mut self, block: u32, data: &[u8]) -> Result<(), FsError>;
    fn set_recovery(&mut self, required: bool) -> Result<(), FsError>;
    fn flush(&mut self) -> Result<(), FsError>;
    fn home_is_allowed(&self, block: u32) -> bool;
}

fn be32(data: &[u8], at: usize) -> Result<u32, FsError> {
    Ok(u32::from_be_bytes(data.get(at..at + 4).ok_or(FsError::Corrupt)?.try_into().map_err(|_| FsError::Corrupt)?))
}
fn put_be32(data: &mut [u8], at: usize, n: u32) { data[at..at + 4].copy_from_slice(&n.to_be_bytes()); }

/// Commit and checkpoint a single classic 32-bit unchecksummed transaction.
///
/// This experimental building block is not a writable filesystem. Its caller
/// must stage *all* changed blocks, prevent cache eviction to their home
/// locations before commit, provide exclusive access, and satisfy the device
/// contract above. On failure, even a returned error can represent a committed
/// transaction; it is never safe to continue without replay. Transactions
/// larger than one descriptor are rejected before any write. When a staged
/// block contains the filesystem superblock, the recovery bit is kept set in
/// both its journal copy and checkpoint until the final marker update.
pub fn commit<I: JournalIo>(
    io: &mut I,
    block_size: usize,
    journal_blocks: u64,
    filesystem_blocks: u32,
    filesystem_super_block: u32,
    uuid: &[u8; 16],
    changes: &[ReplayBlock],
) -> Result<(), FsError> {
    if !(1024..=65536).contains(&block_size) || !block_size.is_power_of_two() || changes.is_empty() {
        return Err(FsError::NotSupported);
    }
    let mut js = io.read_journal(0)?;
    if js.len() != block_size { return Err(FsError::Corrupt); }
    journal::validate_clean(&js, block_size, journal_blocks, uuid)?;
    if be32(&js, 4)? != 4 || be32(&js, 36)? != 0 || be32(&js, 40)? & !1 != 0 || be32(&js, 44)? != 0 {
        return Err(FsError::NotSupported);
    }
    let max = be32(&js, 16)?;
    let first = be32(&js, 20)?;
    let seq = be32(&js, 24)?;
    let needed = changes.len().checked_add(2).ok_or(FsError::NotSupported)?;
    let tag_bytes = changes.len().checked_mul(8).and_then(|n| n.checked_add(16)).ok_or(FsError::NotSupported)?;
    if filesystem_super_block != (1024 / block_size) as u32 || filesystem_super_block >= filesystem_blocks
        || u64::from(max) * block_size as u64 > 64 * 1024 * 1024
        || needed > (max - first) as usize || tag_bytes > block_size - 12 {
        return Err(FsError::NotSupported);
    }
    let mut seen = BTreeSet::new();
    let mut staged = Vec::with_capacity(changes.len());
    for change in changes {
        if change.data.len() != block_size || change.home >= filesystem_blocks
            || !io.home_is_allowed(change.home) || !seen.insert(change.home) {
            return Err(FsError::NotSupported);
        }
        if change.home == filesystem_super_block {
            let offset = 1024 % block_size;
            let sb = change.data.get(offset..offset + 1024).ok_or(FsError::Corrupt)?;
            if sb.get(56..58) != Some(&[0x53, 0xef]) || sb.get(104..120) != Some(uuid.as_slice()) {
                return Err(FsError::Corrupt);
            }
            let mut patched = change.data.clone();
            let at = offset + 96;
            let flags = u32::from_le_bytes(patched[at..at + 4].try_into().map_err(|_| FsError::Corrupt)?);
            patched[at..at + 4].copy_from_slice(&(flags | 4).to_le_bytes());
            staged.push((change.home, Cow::Owned(patched)));
        } else {
            staged.push((change.home, Cow::Borrowed(change.data.as_slice())));
        }
    }

    // Set the recovery bit before any possible durable transaction. No home
    // block can be published before its commit reaches stable media.
    io.set_recovery(true)?;
    io.flush()?;
    let descriptor_slot = first;
    let commit_slot = first + changes.len() as u32 + 1;
    let mut desc = vec![0u8; block_size];
    put_be32(&mut desc, 0, MAGIC);
    put_be32(&mut desc, 4, 1);
    put_be32(&mut desc, 8, seq);
    let mut at = 12;
    for (i, (home, data)) in staged.iter().enumerate() {
        let escape = data[..4] == MAGIC.to_be_bytes();
        let flags = u32::from(escape) | if i == 0 { 0 } else { 2 } | if i + 1 == changes.len() { 8 } else { 0 };
        put_be32(&mut desc, at, *home);
        put_be32(&mut desc, at + 4, flags);
        at += 8;
        if i == 0 { desc[at..at + 16].copy_from_slice(uuid); at += 16; }
    }
    // Invalidate a stale commit at the destination before exposing this log.
    io.write_journal(commit_slot, &vec![0u8; block_size])?;
    io.write_journal(descriptor_slot, &desc)?;
    for (i, (_, data)) in staged.iter().enumerate() {
        if data[..4] == MAGIC.to_be_bytes() {
            let mut escaped = data.to_vec();
            escaped[..4].fill(0);
            io.write_journal(first + 1 + i as u32, &escaped)?;
        } else {
            io.write_journal(first + 1 + i as u32, data)?;
        }
    }
    io.flush()?;
    put_be32(&mut js, 28, descriptor_slot);
    io.write_journal(0, &js)?;
    io.flush()?;
    let mut commit = vec![0u8; block_size];
    put_be32(&mut commit, 0, MAGIC);
    put_be32(&mut commit, 4, 2);
    put_be32(&mut commit, 8, seq);
    io.write_journal(commit_slot, &commit)?;
    io.flush()?;
    for (home, data) in &staged { io.write_home(*home, data)?; }
    io.flush()?;
    put_be32(&mut js, 24, seq.wrapping_add(1));
    put_be32(&mut js, 28, 0);
    io.write_journal(0, &js)?;
    io.flush()?;
    io.set_recovery(false)?;
    io.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, vec};
    const BS: usize = 1024;
    const UUID: [u8; 16] = [0x63; 16];

    #[derive(Clone)]
    struct Image { journal: Vec<Vec<u8>>, home: BTreeMap<u32, Vec<u8>>, recovery: bool }
    struct Sim {
        live: Image,
        durable: Image,
        snapshots: Vec<Image>,
        steps: usize,
        fail_at: Option<usize>,
    }
    impl Sim {
        fn new() -> Self {
            let mut journal = vec![vec![0; BS]; 32];
            put_be32(&mut journal[0], 0, MAGIC); put_be32(&mut journal[0], 4, 4);
            put_be32(&mut journal[0], 12, BS as u32); put_be32(&mut journal[0], 16, 32);
            put_be32(&mut journal[0], 20, 1); put_be32(&mut journal[0], 24, 17);
            journal[0][48..64].copy_from_slice(&UUID);
            let mut home = BTreeMap::new();
            let mut superblock = vec![0; BS];
            superblock[56..58].copy_from_slice(&[0x53, 0xef]);
            superblock[104..120].copy_from_slice(&UUID);
            home.insert(1, superblock);
            home.insert(22, vec![0x11; BS]); home.insert(23, vec![0x22; BS]);
            let image = Image { journal, home, recovery: false };
            Self { live: image.clone(), durable: image.clone(), snapshots: vec![image], steps: 0, fail_at: None }
        }
        fn before(&mut self) -> Result<(), FsError> {
            self.steps += 1;
            if self.fail_at == Some(self.steps) { return Err(FsError::IoError); }
            Ok(())
        }
        fn snapshot(&mut self) {
            // Two legal power-loss outcomes: all writes since the last barrier
            // made it, or none did. Prefixes are captured after every write.
            self.snapshots.push(self.live.clone());
            self.snapshots.push(self.durable.clone());
        }
    }
    impl JournalIo for Sim {
        fn read_journal(&mut self, block: u32) -> Result<Vec<u8>, FsError> { self.before()?; Ok(self.live.journal[block as usize].clone()) }
        fn write_journal(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
            self.before()?; self.live.journal[block as usize].copy_from_slice(data); self.snapshot(); Ok(())
        }
        fn write_home(&mut self, block: u32, data: &[u8]) -> Result<(), FsError> {
            self.before()?;
            if block == 1 { self.live.recovery = data[96] & 4 != 0; }
            self.live.home.insert(block, data.to_vec()); self.snapshot(); Ok(())
        }
        fn set_recovery(&mut self, required: bool) -> Result<(), FsError> {
            self.before()?; self.live.recovery = required;
            if required { self.live.home.get_mut(&1).unwrap()[96] |= 4; }
            else { self.live.home.get_mut(&1).unwrap()[96] &= !4; }
            self.snapshot(); Ok(())
        }
        fn flush(&mut self) -> Result<(), FsError> {
            self.before()?; self.durable = self.live.clone(); self.snapshot(); Ok(())
        }
        fn home_is_allowed(&self, block: u32) -> bool { block == 1 || block >= 16 }
    }
    fn changes() -> Vec<ReplayBlock> {
        let mut a = vec![0xa1; BS]; a[..4].copy_from_slice(&MAGIC.to_be_bytes());
        vec![ReplayBlock { home: 22, data: a }, ReplayBlock { home: 23, data: vec![0xb2; BS] }]
    }
    fn run(sim: &mut Sim) -> Result<(), FsError> { commit(sim, BS, 32, 1024, 1, &UUID, &changes()) }

    #[test]
    fn every_write_and_barrier_crash_recovers_the_old_or_committed_state() {
        let mut sim = Sim::new();
        run(&mut sim).unwrap();
        let expected = changes();
        for (index, image) in sim.snapshots.iter().enumerate() {
            let mut homes = image.home.clone();
            let start = be32(&image.journal[0], 28).unwrap();
            if image.recovery && start != 0 {
                let plan = journal::plan_replay(&image.journal[0], BS, 32, 1024, &UUID, |n| Ok(image.journal[n as usize].clone())).unwrap();
                for entry in plan { homes.insert(entry.home, entry.data); }
            }
            let old = homes[&22] == vec![0x11; BS] && homes[&23] == vec![0x22; BS];
            let new = homes[&22] == expected[0].data && homes[&23] == expected[1].data;
            assert!(old || new, "inconsistent crash snapshot {index}");
        }
        assert!(!sim.live.recovery);
        assert_eq!(sim.live.home[&22], expected[0].data);
        assert_eq!(sim.live.home[&23], expected[1].data);
        assert_eq!(be32(&sim.live.journal[0], 24).unwrap(), 18);
        assert_eq!(be32(&sim.live.journal[0], 28).unwrap(), 0);
    }

    #[test]
    fn io_errors_abort_and_invalid_transactions_make_no_writes() {
        let mut baseline = Sim::new(); run(&mut baseline).unwrap();
        for fail_at in 1..=baseline.steps {
            let mut sim = Sim::new(); sim.fail_at = Some(fail_at);
            assert_eq!(run(&mut sim), Err(FsError::IoError), "step {fail_at}");
            assert_eq!(sim.steps, fail_at, "I/O continued after failure");
        }
        let mut sim = Sim::new();
        let invalid = vec![ReplayBlock { home: 22, data: vec![0; BS] }, ReplayBlock { home: 22, data: vec![0; BS] }];
        assert_eq!(commit(&mut sim, BS, 32, 1024, 1, &UUID, &invalid), Err(FsError::NotSupported));
        assert_eq!(sim.snapshots.len(), 1, "validation must precede writes");
    }

    #[test]
    fn checkpoint_keeps_recovery_bit_when_superblock_itself_changes() {
        let mut sim = Sim::new();
        let mut changed_super = sim.live.home[&1].clone();
        changed_super[12] = 77; // Representative allocation count update.
        let changes = [ReplayBlock { home: 1, data: changed_super }, ReplayBlock { home: 22, data: vec![0xa1; BS] }];
        commit(&mut sim, BS, 32, 1024, 1, &UUID, &changes).unwrap();
        for image in &sim.snapshots {
            let block = &image.home[&1];
            assert_eq!(block[96] & 4 != 0, image.recovery);
            let start = be32(&image.journal[0], 28).unwrap();
            if image.recovery && start != 0 {
                let plan = journal::plan_replay(&image.journal[0], BS, 32, 1024, &UUID, |n| Ok(image.journal[n as usize].clone())).unwrap();
                if let Some(sb) = plan.iter().find(|entry| entry.home == 1) {
                    assert_ne!(sb.data[96] & 4, 0, "replay must preserve the recovery marker");
                }
            }
        }
        assert_eq!(sim.live.home[&1][12], 77);
        assert_eq!(sim.live.home[&1][96] & 4, 0);
    }

    #[test]
    fn written_ext3_transaction_is_replayed_by_e2fsck() {
        use std::process::Command;
        struct ImageIo { bytes: Vec<u8>, journal: [usize; 5], fail_home: bool }
        impl JournalIo for ImageIo {
            fn read_journal(&mut self, n: u32) -> Result<Vec<u8>, FsError> {
                let p = *self.journal.get(n as usize).ok_or(FsError::IoError)? * BS;
                Ok(self.bytes[p..p + BS].to_vec())
            }
            fn write_journal(&mut self, n: u32, data: &[u8]) -> Result<(), FsError> {
                let p = *self.journal.get(n as usize).ok_or(FsError::IoError)? * BS;
                self.bytes[p..p + BS].copy_from_slice(data); Ok(())
            }
            fn write_home(&mut self, n: u32, data: &[u8]) -> Result<(), FsError> {
                if self.fail_home { return Err(FsError::IoError); }
                let p = n as usize * BS; self.bytes[p..p + BS].copy_from_slice(data); Ok(())
            }
            fn set_recovery(&mut self, required: bool) -> Result<(), FsError> {
                let at = 1024 + 96;
                let flags = u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap());
                let flags = if required { flags | 4 } else { flags & !4 };
                self.bytes[at..at + 4].copy_from_slice(&flags.to_le_bytes()); Ok(())
            }
            fn flush(&mut self) -> Result<(), FsError> { Ok(()) }
            fn home_is_allowed(&self, n: u32) -> bool { !self.journal.contains(&(n as usize)) }
        }
        let dir = std::env::temp_dir().join(std::format!("akuma-jbd-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("disk.img"); let input = dir.join("payload");
        std::fs::File::create(&image).unwrap().set_len(32 * 1024 * 1024).unwrap();
        std::fs::write(&input, b"old-content\n").unwrap();
        assert!(Command::new("mke2fs").args(["-q", "-F", "-t", "ext3", "-b", "1024"]).arg(&image).status().unwrap().success());
        let command = std::format!("write {} /payload", input.display());
        assert!(Command::new("debugfs").args(["-w", "-R", &command]).arg(&image).output().unwrap().status.success());
        let bmap = |command: &str| -> usize {
            let result = Command::new("debugfs").args(["-R", command]).arg(&image).output().unwrap();
            assert!(result.status.success());
            std::str::from_utf8(&result.stdout).unwrap().trim().parse().unwrap()
        };
        let home = bmap("bmap /payload 0");
        let mapping = [bmap("bmap <8> 0"), bmap("bmap <8> 1"), bmap("bmap <8> 2"), bmap("bmap <8> 3"), bmap("bmap <8> 4")];
        let original = std::fs::read(&image).unwrap();
        let uuid: [u8; 16] = original[1024 + 104..1024 + 120].try_into().unwrap();
        let journal_blocks = u64::from(be32(&original[mapping[0] * BS..], 16).unwrap());
        let mut data = vec![0; BS]; data[..12].copy_from_slice(b"new-content\n");
        let mut superblock = original[BS..2 * BS].to_vec();
        superblock[44..48].copy_from_slice(&1234u32.to_le_bytes());
        let changes = [ReplayBlock { home: home as u32, data }, ReplayBlock { home: 1, data: superblock }];
        // A failure at the first checkpoint leaves a fully committed log.
        let mut io = ImageIo { bytes: original.clone(), journal: mapping, fail_home: true };
        assert_eq!(commit(&mut io, BS, journal_blocks, (original.len() / BS) as u32, 1, &uuid, &changes), Err(FsError::IoError));
        std::fs::write(&image, &io.bytes).unwrap();
        let status = Command::new("e2fsck").args(["-fy", "-E", "journal_only"]).arg(&image).output().unwrap().status;
        assert!(matches!(status.code(), Some(0 | 1)));
        let recovered = Command::new("debugfs").args(["-R", "cat /payload"]).arg(&image).output().unwrap();
        assert_eq!(recovered.stdout, b"new-content\n");
        let replayed = std::fs::read(&image).unwrap();
        assert_eq!(u32::from_le_bytes(replayed[1024 + 44..1024 + 48].try_into().unwrap()), 1234);
        let status = Command::new("e2fsck").args(["-fn"]).arg(&image).output().unwrap().status;
        assert!(status.success());
        // A fully checkpointed transaction must also leave a clean image.
        let mut io = ImageIo { bytes: original.clone(), journal: mapping, fail_home: false };
        commit(&mut io, BS, journal_blocks, (original.len() / BS) as u32, 1, &uuid, &changes).unwrap();
        assert_eq!(u32::from_le_bytes(io.bytes[1024 + 44..1024 + 48].try_into().unwrap()), 1234);
        std::fs::write(&image, &io.bytes).unwrap();
        let status = Command::new("e2fsck").args(["-fn"]).arg(&image).output().unwrap().status;
        assert!(status.success());
        let recovered = Command::new("debugfs").args(["-R", "cat /payload"]).arg(&image).output().unwrap();
        assert_eq!(recovered.stdout, b"new-content\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
