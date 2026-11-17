//! Independently implemented ext4 metadata checksums. CRC32C uses the on-disk
//! convention: initial all-ones state and no final xor. Format reference:
//! https://docs.kernel.org/filesystems/ext4/checksums.html

/// Update a reflected CRC32C state (Castagnoli polynomial).
pub(super) fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    crc
}

pub(super) fn seed(uuid: &[u8; 16]) -> u32 { crc32c(!0, uuid) }

pub(super) fn inode_seed(seed: u32, number: u32, generation: u32) -> u32 {
    crc32c(crc32c(seed, &number.to_le_bytes()), &generation.to_le_bytes())
}

pub(super) fn superblock(data: &[u8]) -> Option<bool> {
    let expected = u32::from_le_bytes(data.get(1020..1024)?.try_into().ok()?);
    Some(crc32c(!0, data.get(..1020)?) == expected)
}

pub(super) fn group(seed: u32, number: u32, data: &[u8]) -> Option<bool> {
    let expected = u16::from_le_bytes(data.get(30..32)?.try_into().ok()?);
    let mut crc = crc32c(seed, &number.to_le_bytes());
    crc = crc32c(crc, data.get(..30)?);
    crc = crc32c(crc, &[0, 0]);
    crc = crc32c(crc, data.get(32..)?);
    Some(crc as u16 == expected)
}

pub(super) fn inode(seed: u32, number: u32, data: &[u8]) -> Option<bool> {
    let generation = u32::from_le_bytes(data.get(100..104)?.try_into().ok()?);
    let low = u16::from_le_bytes(data.get(124..126)?.try_into().ok()?);
    let has_high = data.len() >= 132 && u16::from_le_bytes(data.get(128..130)?.try_into().ok()?) >= 4;
    let high = if has_high { u16::from_le_bytes(data.get(130..132)?.try_into().ok()?) } else { 0 };
    let mut crc = inode_seed(seed, number, generation);
    crc = crc32c(crc, data.get(..124)?);
    crc = crc32c(crc, &[0, 0]);
    if has_high {
        crc = crc32c(crc, data.get(126..130)?);
        crc = crc32c(crc, &[0, 0]);
        crc = crc32c(crc, data.get(132..)?);
        Some(crc == u32::from(low) | (u32::from(high) << 16))
    } else {
        crc = crc32c(crc, data.get(126..)?);
        Some(crc as u16 == low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc32c_known_vector_and_incremental_updates() {
        assert_eq!(!crc32c(!0, b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(crc32c(!0, b"1234"), b"56789"), crc32c(!0, b"123456789"));
    }
}
