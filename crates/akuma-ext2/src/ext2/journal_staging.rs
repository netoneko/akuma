//! Block-granular transaction staging for the journal writer. No home writes
//! happen here. A failed staging call leaves the existing write set unchanged.

use alloc::{collections::BTreeMap, vec::Vec};
use akuma_vfs::FsError;
use super::ReplayBlock;

/// A bounded set of whole-block after-images, suitable for one JBD transaction.
/// It does not itself provide filesystem rollback or concurrency control.
#[derive(Clone)]
pub struct WriteSet {
    block_size: usize,
    filesystem_blocks: u32,
    max_blocks: usize,
    blocks: BTreeMap<u32, Vec<u8>>,
}

impl WriteSet {
    pub fn new(block_size: usize, filesystem_blocks: u32, max_blocks: usize) -> Result<Self, FsError> {
        if !(1024..=65536).contains(&block_size) || !block_size.is_power_of_two() || filesystem_blocks == 0 || max_blocks == 0 {
            return Err(FsError::NotSupported);
        }
        Ok(Self { block_size, filesystem_blocks, max_blocks, blocks: BTreeMap::new() })
    }

    /// Stage bytes, loading each missing block through `read_home`. All bounds,
    /// capacity and reads are checked before the write set changes.
    pub fn stage(
        &mut self,
        offset: u64,
        data: &[u8],
        mut read_home: impl FnMut(u32) -> Result<Vec<u8>, FsError>,
    ) -> Result<(), FsError> {
        if data.is_empty() { return Ok(()); }
        let end = offset.checked_add(data.len() as u64).ok_or(FsError::NotSupported)?;
        let bs = self.block_size as u64;
        let first = u32::try_from(offset / bs).map_err(|_| FsError::NotSupported)?;
        let last = u32::try_from((end - 1) / bs).map_err(|_| FsError::NotSupported)?;
        if last >= self.filesystem_blocks { return Err(FsError::NotSupported); }
        let missing_count = (first..=last).filter(|n| !self.blocks.contains_key(n)).count();
        if self.blocks.len().checked_add(missing_count).ok_or(FsError::NoSpace)? > self.max_blocks {
            return Err(FsError::NoSpace);
        }
        let mut missing = Vec::with_capacity(missing_count);
        for block in first..=last {
            if !self.blocks.contains_key(&block) {
                let bytes = read_home(block)?;
                if bytes.len() != self.block_size { return Err(FsError::Corrupt); }
                missing.push((block, bytes));
            }
        }
        for (block, bytes) in missing { self.blocks.insert(block, bytes); }
        let mut consumed = 0;
        for block in first..=last {
            let block_start = u64::from(block) * bs;
            let from = offset.max(block_start);
            let to = end.min(block_start + bs);
            let n = (to - from) as usize;
            let dst = (from - block_start) as usize;
            self.blocks.get_mut(&block).expect("staged above")[dst..dst + n].copy_from_slice(&data[consumed..consumed + n]);
            consumed += n;
        }
        Ok(())
    }

    /// Read a staged block, if present; absent blocks must be read from home.
    pub fn get(&self, block: u32) -> Option<&[u8]> { self.blocks.get(&block).map(Vec::as_slice) }

    /// Return the staged after-images, consuming the write set.
    pub fn into_blocks(self) -> Vec<ReplayBlock> {
        self.blocks.into_iter().map(|(home, data)| ReplayBlock { home, data }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn partial_overlapping_writes_preserve_unmodified_bytes() {
        let mut set = WriteSet::new(1024, 16, 3).unwrap();
        set.stage(1022, b"abcd", |_| Ok(vec![0x5a; 1024])).unwrap();
        set.stage(1023, b"12", |_| panic!("already staged")).unwrap();
        assert_eq!(&set.get(0).unwrap()[1020..], &[0x5a, 0x5a, b'a', b'1']);
        assert_eq!(&set.get(1).unwrap()[..4], &[b'2', b'd', 0x5a, 0x5a]);
        assert_eq!(set.into_blocks().len(), 2);
    }

    #[test]
    fn capacity_read_failure_and_bounds_do_not_partially_stage() {
        let mut set = WriteSet::new(1024, 16, 2).unwrap();
        set.stage(0, b"prior", |_| Ok(vec![0; 1024])).unwrap();
        assert_eq!(set.stage(1024, &vec![1; 2048], |_| Ok(vec![0; 1024])), Err(FsError::NoSpace));
        assert!(set.get(1).is_none());
        assert_eq!(set.stage(1024, b"new", |_| Err(FsError::IoError)), Err(FsError::IoError));
        assert!(set.get(1).is_none());
        assert_eq!(&set.get(0).unwrap()[..5], b"prior");
        assert_eq!(set.stage(16 * 1024 - 1, b"xx", |_| Ok(vec![0; 1024])), Err(FsError::NotSupported));
        assert!(set.get(15).is_none());
    }
}
