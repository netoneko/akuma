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
