//! JBD/JBD2 superblock codec, implemented from the public on-disk format.
//! Journal integers are big-endian, unlike the containing ext filesystem.

use akuma_vfs::FsError;

const MAGIC: u32 = 0xc03b_3998;

fn be32(data: &[u8], offset: usize) -> Result<u32, FsError> {
    Ok(u32::from_be_bytes(data.get(offset..offset + 4).ok_or(FsError::Corrupt)?.try_into().map_err(|_| FsError::Corrupt)?))
}

/// Check a journal that is claimed clean by the filesystem superblock. This
/// only recognizes a safely empty internal journal; it does not replay one.
pub(super) fn validate_clean(data: &[u8], block_size: usize, blocks: u64, uuid: &[u8; 16]) -> Result<(), FsError> {
    if be32(data, 0)? != MAGIC || !matches!(be32(data, 4)?, 3 | 4) { return Err(FsError::Corrupt); }
    if be32(data, 12)? as usize != block_size { return Err(FsError::Corrupt); }
    let max = be32(data, 16)?;
    let first = be32(data, 20)?;
    if max < 2 || u64::from(max) > blocks || first == 0 || first >= max { return Err(FsError::Corrupt); }
    // A nonzero start requires a replay which this stage does not implement.
    // Zero on its own is not proof of cleanliness: the caller also checks
    // the filesystem's clean, recovery and orphan state.
    if be32(data, 28)? != 0 || be32(data, 32)? != 0 { return Err(FsError::NotSupported); }
    if data.get(48..64) != Some(uuid.as_slice()) { return Err(FsError::Corrupt); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put(data: &mut [u8], off: usize, n: u32) { data[off..off + 4].copy_from_slice(&n.to_be_bytes()); }
    #[test]
    fn clean_superblock_rejects_dirty_invalid_and_foreign_journals() {
        let mut block = [0u8; 1024];
        let uuid = [0x5a; 16];
        put(&mut block, 0, MAGIC); put(&mut block, 4, 4); put(&mut block, 12, 1024);
        put(&mut block, 16, 128); put(&mut block, 20, 1); block[48..64].copy_from_slice(&uuid);
        assert_eq!(validate_clean(&block, 1024, 128, &uuid), Ok(()));
        put(&mut block, 28, 1);
        assert_eq!(validate_clean(&block, 1024, 128, &uuid), Err(FsError::NotSupported));
        put(&mut block, 28, 0); put(&mut block, 20, 128);
        assert_eq!(validate_clean(&block, 1024, 128, &uuid), Err(FsError::Corrupt));
        put(&mut block, 20, 1); block[48] ^= 1;
        assert_eq!(validate_clean(&block, 1024, 128, &uuid), Err(FsError::Corrupt));
    }
}

/// A committed journal block to be applied to its home location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayBlock {
    pub home: u32,
    pub data: alloc::vec::Vec<u8>,
}

/// Inspect a classic 32-bit, unchecksummed internal journal and produce a
/// replay plan without writing to the device. `read_journal` addresses blocks
/// by their logical position in the journal file. Unsupported JBD2 features are
/// rejected; callers must still enforce filesystem recovery and durability
/// rules before applying a plan. This is not a complete recovery implementation.
pub fn plan_replay(
    superblock: &[u8],
    block_size: usize,
    journal_blocks: u64,
    filesystem_blocks: u32,
    uuid: &[u8; 16],
    mut read_journal: impl FnMut(u32) -> Result<alloc::vec::Vec<u8>, FsError>,
) -> Result<alloc::vec::Vec<ReplayBlock>, FsError> {
    use alloc::{collections::BTreeMap, vec::Vec};

    if !(1024..=65536).contains(&block_size) || !block_size.is_power_of_two() {
        return Err(FsError::NotSupported);
    }
    if be32(superblock, 0)? != MAGIC || be32(superblock, 4)? != 4
        || be32(superblock, 12)? as usize != block_size
        || superblock.get(48..64) != Some(uuid.as_slice()) {
        return Err(FsError::Corrupt);
    }
    let max = be32(superblock, 16)?;
    let first = be32(superblock, 20)?;
    let start = be32(superblock, 28)?;
    let mut sequence = be32(superblock, 24)?;
    if max < 2 || u64::from(max) > journal_blocks || first == 0 || first >= max {
        return Err(FsError::Corrupt);
    }
    if u64::from(max) * block_size as u64 > 64 * 1024 * 1024 {
        return Err(FsError::NotSupported);
    }
    // This initial implementation covers classic JBD descriptors and revokes.
    // Never accept checksummed, 64-bit, asynchronous or fast-commit records as
    // if they were this format.
    if be32(superblock, 32)? != 0 || be32(superblock, 36)? != 0
        || be32(superblock, 40)? & !1 != 0 || be32(superblock, 44)? != 0 {
        return Err(FsError::NotSupported);
    }
    if start == 0 { return Ok(Vec::new()); }
    if start < first || start >= max { return Err(FsError::Corrupt); }
    let mut pos = start;
    let mut consumed = 0u32;
    let capacity = max - first;
    let mut committed: Vec<(u64, ReplayBlock)> = Vec::new();
    let mut committed_revokes = BTreeMap::<u32, u64>::new();
    let mut pending = Vec::new();
    let mut pending_revokes = Vec::new();
    let mut ordinal = 0u64;
    let mut saw_record = false;
    while consumed < capacity {
        let block = read_journal(pos)?;
        if block.len() != block_size { return Err(FsError::Corrupt); }
        consumed += 1;
        pos += 1;
        if pos == max { pos = first; }
        if be32(&block, 0)? != MAGIC || be32(&block, 8)? != sequence { break; }
        match be32(&block, 4)? {
            1 => {
                saw_record = true;
                let mut at = 12usize;
                loop {
                    let home = be32(&block, at)?;
                    let flags = be32(&block, at + 4)?;
                    at = at.checked_add(8).ok_or(FsError::Corrupt)?;
                    if flags & !0xb != 0 || home >= filesystem_blocks { return Err(FsError::NotSupported); }
                    if flags & 2 == 0 {
                        let end = at.checked_add(16).ok_or(FsError::Corrupt)?;
                        if block.get(at..end) != Some(uuid.as_slice()) { return Err(FsError::Corrupt); }
                        at = end;
                    }
                    if consumed >= capacity { return Err(FsError::Corrupt); }
                    let mut data = read_journal(pos)?;
                    if data.len() != block_size { return Err(FsError::Corrupt); }
                    consumed += 1;
                    pos += 1;
                    if pos == max { pos = first; }
                    if flags & 1 != 0 { data[..4].copy_from_slice(&MAGIC.to_be_bytes()); }
                    pending.push(ReplayBlock { home, data });
                    if flags & 8 != 0 { break; }
                }
            }
            5 => {
                saw_record = true;
                if be32(superblock, 40)? & 1 == 0 { return Err(FsError::Corrupt); }
                let count = be32(&block, 12)? as usize;
                if count < 16 || count > block_size || (count - 16) % 4 != 0 { return Err(FsError::Corrupt); }
                for at in (16..count).step_by(4) {
                    let home = be32(&block, at)?;
                    if home >= filesystem_blocks { return Err(FsError::Corrupt); }
                    pending_revokes.push(home);
                }
            }
            2 => {
                if !saw_record { return Err(FsError::Corrupt); }
                for home in pending_revokes.drain(..) { committed_revokes.insert(home, ordinal); }
                committed.extend(pending.drain(..).map(|entry| (ordinal, entry)));
                ordinal += 1;
                sequence = sequence.wrapping_add(1);
                saw_record = false;
            }
            _ => return Err(FsError::NotSupported),
        }
    }
    // An incomplete final transaction is discarded; later committed revokes
    // suppress older logged versions, including across transaction ID wrap.
    let mut result = BTreeMap::new();
    for (tx, entry) in committed {
        if committed_revokes.get(&entry.home).is_none_or(|&revoke| revoke < tx) {
            result.insert(entry.home, entry.data);
        }
    }
    Ok(result.into_iter().map(|(home, data)| ReplayBlock { home, data }).collect())
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    const BS: usize = 1024;
    const UUID: [u8; 16] = [0x42; 16];
    fn put(b: &mut [u8], at: usize, n: u32) { b[at..at + 4].copy_from_slice(&n.to_be_bytes()); }
    fn header(kind: u32, seq: u32) -> Vec<u8> {
        let mut b = vec![0; BS]; put(&mut b, 0, MAGIC); put(&mut b, 4, kind); put(&mut b, 8, seq); b
    }
    fn superblock(start: u32, sequence: u32) -> Vec<u8> {
        let mut b = header(4, 0); put(&mut b, 12, BS as u32); put(&mut b, 16, 16);
        put(&mut b, 20, 1); put(&mut b, 24, sequence); put(&mut b, 28, start);
        put(&mut b, 40, 1); b[48..64].copy_from_slice(&UUID); b
    }
    fn desc(seq: u32, target: u32, flags: u32) -> Vec<u8> {
        let mut b = header(1, seq); put(&mut b, 12, target); put(&mut b, 16, flags | 8);
        if flags & 2 == 0 { b[20..36].copy_from_slice(&UUID); } b
    }
    fn run(blocks: &[Vec<u8>], start: u32, seq: u32) -> Result<Vec<ReplayBlock>, FsError> {
        plan_replay(&superblock(start, seq), BS, 16, 4096, &UUID, |n| Ok(blocks[n as usize].clone()))
    }
    #[test]
    fn committed_data_survives_and_incomplete_transaction_is_discarded() {
        let mut b = vec![vec![0; BS]; 16];
        b[1] = desc(9, 77, 0); b[2] = vec![0x51; BS]; b[3] = header(2, 9);
        b[4] = desc(10, 88, 0); b[5] = vec![0x52; BS];
        let plan = run(&b, 1, 9).unwrap();
        assert_eq!(plan, vec![ReplayBlock { home: 77, data: vec![0x51; BS] }]);
    }
    #[test]
    fn later_revoke_suppresses_old_data_across_sequence_and_ring_wrap() {
        let mut b = vec![vec![0; BS]; 16];
        b[14] = desc(u32::MAX, 77, 0); b[15] = vec![0x51; BS]; b[1] = header(2, u32::MAX);
        b[2] = header(5, 0); put(&mut b[2], 12, 20); put(&mut b[2], 16, 77); b[3] = header(2, 0);
        b[4] = desc(1, 88, 1); b[5] = vec![0x52; BS]; b[6] = header(2, 1);
        let plan = run(&b, 14, u32::MAX).unwrap();
        assert_eq!(plan.len(), 1); assert_eq!(plan[0].home, 88);
        assert_eq!(&plan[0].data[..4], &MAGIC.to_be_bytes());
    }
    #[test]
    fn invalid_records_and_unsupported_journal_features_are_rejected() {
        let mut b = vec![vec![0; BS]; 16]; b[1] = desc(1, 4096, 0);
        assert_eq!(run(&b, 1, 1), Err(FsError::NotSupported));
        b[1] = desc(1, 1, 0); b[1][20] ^= 1;
        assert_eq!(run(&b, 1, 1), Err(FsError::Corrupt));
        let mut sb = superblock(1, 1); put(&mut sb, 40, 0x10);
        assert_eq!(plan_replay(&sb, BS, 16, 4096, &UUID, |_| Err(FsError::IoError)), Err(FsError::NotSupported));
    }

    #[test]
    fn overlay_reads_across_replayed_and_untouched_blocks_without_writing() {
        use alloc::collections::BTreeMap;
        use crate::BlockDevice;
        struct Device;
        impl BlockDevice for Device {
            fn read_bytes(&self, _: u64, out: &mut [u8]) -> Result<(), ()> { out.fill(0x11); Ok(()) }
            fn write_bytes(&self, _: u64, _: &[u8]) -> Result<(), ()> { panic!("read-only overlay wrote the device") }
        }
        let mut blocks = BTreeMap::new();
        blocks.insert(1, vec![0x22; BS]);
        let dev = crate::RecoveryDevice { dev: Device, block_size: BS, blocks };
        let mut buf = vec![0; BS + 4];
        dev.read_bytes((BS - 2) as u64, &mut buf).unwrap();
        assert_eq!(&buf[..2], &[0x11; 2]);
        assert!(buf[2..BS + 2].iter().all(|&b| b == 0x22));
        assert_eq!(&buf[BS + 2..], &[0x11; 2]);
        assert!(dev.write_bytes(0, b"x").is_err());
    }

    #[test]
    fn replay_plan_matches_e2fsck_on_a_real_ext3_image() {
        use std::process::Command;
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicUsize, Ordering};
        use akuma_vfs::Filesystem;
        struct ReadOnlyImage { bytes: Vec<u8>, writes: Arc<AtomicUsize> }
        impl crate::BlockDevice for ReadOnlyImage {
            fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
                let start = usize::try_from(offset).map_err(|_| ())?;
                let end = start.checked_add(buf.len()).ok_or(())?;
                buf.copy_from_slice(self.bytes.get(start..end).ok_or(())?);
                Ok(())
            }
            fn write_bytes(&self, _: u64, _: &[u8]) -> Result<(), ()> {
                self.writes.fetch_add(1, Ordering::Relaxed);
                Err(())
            }
        }
        let dir = std::env::temp_dir().join(std::format!("akuma-jbd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("disk.img");
        let payload = dir.join("payload");
        std::fs::File::create(&image).unwrap().set_len(32 * 1024 * 1024).unwrap();
        std::fs::write(&payload, b"old-content\n").unwrap();
        assert!(Command::new("mke2fs").args(["-q", "-F", "-t", "ext3", "-b", "1024"]).arg(&image).status().unwrap().success());
        let command = std::format!("write {} /payload", payload.display());
        assert!(Command::new("debugfs").args(["-w", "-R", &command]).arg(&image).output().unwrap().status.success());
        let bmap = |command: &str| -> usize {
            let result = Command::new("debugfs").args(["-R", command]).arg(&image).output().unwrap();
            assert!(result.status.success());
            std::str::from_utf8(&result.stdout).unwrap().trim().parse().unwrap()
        };
        let home = bmap("bmap /payload 0");
        let j = [bmap("bmap <8> 0"), bmap("bmap <8> 1"), bmap("bmap <8> 2"), bmap("bmap <8> 3")];
        let mut image_bytes = std::fs::read(&image).unwrap();
        let uuid: [u8; 16] = image_bytes[1024 + 104..1024 + 120].try_into().unwrap();
        let feature = u32::from_le_bytes(image_bytes[1024 + 96..1024 + 100].try_into().unwrap()) | 4;
        image_bytes[1024 + 96..1024 + 100].copy_from_slice(&feature.to_le_bytes());
        let js = j[0] * BS;
        put(&mut image_bytes[js..js + BS], 24, 1);
        put(&mut image_bytes[js..js + BS], 28, 1);
        let mut descriptor = header(1, 1);
        put(&mut descriptor, 12, home as u32); put(&mut descriptor, 16, 8);
        descriptor[20..36].copy_from_slice(&uuid);
        image_bytes[j[1] * BS..(j[1] + 1) * BS].copy_from_slice(&descriptor);
        let mut new_data = vec![0; BS]; new_data[..12].copy_from_slice(b"new-content\n");
        image_bytes[j[2] * BS..(j[2] + 1) * BS].copy_from_slice(&new_data);
        image_bytes[j[3] * BS..(j[3] + 1) * BS].copy_from_slice(&header(2, 1));
        // Derive the mapped blocks from debugfs rather than assuming the
        // journal is physically contiguous. The first empty header ends scan.
        let mut journal_blocks = j.to_vec();
        journal_blocks.push(bmap("bmap <8> 4"));
        let max = be32(&image_bytes[js..js + BS], 16).unwrap();
        let plan = plan_replay(&image_bytes[js..js + BS], BS, u64::from(max), (image_bytes.len() / BS) as u32, &uuid, |index| {
            let physical = *journal_blocks.get(index as usize).ok_or(FsError::IoError)?;
            Ok(image_bytes[physical * BS..(physical + 1) * BS].to_vec())
        }).unwrap();
        assert_eq!(plan, vec![ReplayBlock { home: home as u32, data: new_data }]);
        let writes = Arc::new(AtomicUsize::new(0));
        let recovered_fs = crate::Ext2Filesystem::recover_read_only(ReadOnlyImage { bytes: image_bytes.clone(), writes: writes.clone() }, || 0, None).unwrap();
        assert_eq!(recovered_fs.read_file("/payload").unwrap(), b"new-content\n");
        assert_eq!(recovered_fs.write_file("/payload", b"x"), Err(FsError::ReadOnly));
        recovered_fs.sync().unwrap();
        assert_eq!(writes.load(Ordering::Relaxed), 0);
        std::fs::write(&image, &image_bytes).unwrap();
        let check = Command::new("e2fsck").args(["-fy", "-E", "journal_only"]).arg(&image).output().unwrap();
        assert!(matches!(check.status.code(), Some(0 | 1)));
        let recovered = Command::new("debugfs").args(["-R", "cat /payload"]).arg(&image).output().unwrap();
        assert_eq!(recovered.stdout, &plan[0].data[..12]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
