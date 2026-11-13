//! Feature negotiation (virtio 1.x §2.2, §6).
//!
//! The driver accepts the intersection of what the device offers and what it
//! asks for, and refuses a device that does not offer [`VERSION_1`]: without it
//! the device expects the legacy layout and header sizes, which this driver
//! never produces.

/// The device speaks virtio 1.x (modern layout, little-endian, 12-byte net header).
pub const VERSION_1: u64 = 1 << 32;

/// Why a device was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The device does not offer [`VERSION_1`].
    NotModern,
}

/// The feature word to write back: `wanted` (plus [`VERSION_1`]) as far as the
/// device offers it.
pub const fn negotiate(offered: u64, wanted: u64) -> Result<u64, Refused> {
    if offered & VERSION_1 == 0 {
        return Err(Refused::NotModern);
    }
    Ok(offered & (wanted | VERSION_1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_without_version_1_is_refused() {
        assert_eq!(negotiate(0xffff_ffff, 0xffff_ffff), Err(Refused::NotModern));
    }

    #[test]
    fn only_what_both_sides_name_is_accepted() {
        let offered = VERSION_1 | 1 << 5 | 1 << 9;
        assert_eq!(negotiate(offered, 1 << 5 | 1 << 6), Ok(VERSION_1 | 1 << 5));
        assert_eq!(negotiate(offered, 0), Ok(VERSION_1));
    }
}
