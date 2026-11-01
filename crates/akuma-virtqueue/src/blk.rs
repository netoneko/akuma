//! virtio-blk on the wire (virtio 1.x §5.2).
//!
//! A request is three buffers: a 16-byte header the device reads, the data
//! (device-written for a read, device-read for a write), and one status byte
//! the device writes last.

/// Bytes per sector: the unit of the header's `sector` and of [`CAPACITY`],
/// whatever the device's block size.
pub const SECTOR_SIZE: usize = 512;
/// Config offset of `capacity`, a `u64` count of [`SECTOR_SIZE`] sectors.
pub const CAPACITY: usize = 0;
/// Feature: the device is read-only.
pub const F_RO: u64 = 1 << 5;

pub const T_IN: u32 = 0;
pub const T_OUT: u32 = 1;

pub const HEADER_LEN: usize = 16;

pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

/// The request header: `type u32 | reserved u32 | sector u64`, little-endian.
#[must_use]
pub const fn header(kind: u32, sector: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    let t = kind.to_le_bytes();
    let s = sector.to_le_bytes();
    let mut i = 0;
    while i < 4 {
        h[i] = t[i];
        i += 1;
    }
    let mut i = 0;
    while i < 8 {
        h[8 + i] = s[i];
        i += 1;
    }
    h
}

/// Whether `sectors` sectors starting at `sector` lie inside a device of
/// `capacity` sectors, without overflowing.
#[must_use]
pub const fn in_range(sector: u64, sectors: u64, capacity: u64) -> bool {
    match sector.checked_add(sectors) {
        Some(end) => end <= capacity,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_type_reserved_sector_little_endian() {
        assert_eq!(header(T_OUT, 0x0102_0304_0506_0708), [1, 0, 0, 0, 0, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(header(T_IN, 0), [0; 16]);
    }

    #[test]
    fn range_check_does_not_wrap() {
        assert!(in_range(0, 8, 8));
        assert!(!in_range(1, 8, 8));
        assert!(!in_range(u64::MAX, 2, u64::MAX));
    }
}
