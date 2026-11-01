//! The virtio-mmio register map, version 2 ("modern") only (virtio 1.x §4.2.2).
//!
//! Offsets are bytes from the slot base. Every register is 32 bits wide; the
//! device-specific configuration space starts at [`CONFIG`].

/// `"virt"` little-endian: what [`MAGIC_VALUE`] reads on any virtio-mmio slot.
pub const MAGIC: u32 = 0x7472_6976;
/// The only transport version this driver speaks. Version 1 (legacy) was
/// dropped with QEMU's `force-legacy` flag (`docs/VIRTIO_MMIO_LEGACY_TO_MODERN.md`).
pub const VERSION_MODERN: u32 = 2;

pub const MAGIC_VALUE: usize = 0x000;
pub const VERSION: usize = 0x004;
/// 0 = no device behind this slot.
pub const DEVICE_ID: usize = 0x008;
pub const VENDOR_ID: usize = 0x00c;
pub const DEVICE_FEATURES: usize = 0x010;
pub const DEVICE_FEATURES_SEL: usize = 0x014;
pub const DRIVER_FEATURES: usize = 0x020;
pub const DRIVER_FEATURES_SEL: usize = 0x024;
pub const QUEUE_SEL: usize = 0x030;
pub const QUEUE_NUM_MAX: usize = 0x034;
pub const QUEUE_NUM: usize = 0x038;
pub const QUEUE_READY: usize = 0x044;
pub const QUEUE_NOTIFY: usize = 0x050;
pub const INTERRUPT_STATUS: usize = 0x060;
pub const INTERRUPT_ACK: usize = 0x064;
pub const STATUS: usize = 0x070;
pub const QUEUE_DESC_LOW: usize = 0x080;
pub const QUEUE_DESC_HIGH: usize = 0x084;
/// The available ring ("driver area").
pub const QUEUE_DRIVER_LOW: usize = 0x090;
pub const QUEUE_DRIVER_HIGH: usize = 0x094;
/// The used ring ("device area").
pub const QUEUE_DEVICE_LOW: usize = 0x0a0;
pub const QUEUE_DEVICE_HIGH: usize = 0x0a4;
pub const CONFIG_GENERATION: usize = 0x0fc;
pub const CONFIG: usize = 0x100;

/// Device ids (virtio 1.x §5), the ones this tree looks for.
pub mod device_id {
    pub const NET: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const RNG: u32 = 4;
    pub const SOUND: u32 = 25;
}

/// Whether a slot's first two registers describe a device this driver can run.
#[must_use]
pub const fn is_modern(magic: u32, version: u32) -> bool {
    magic == MAGIC && version == VERSION_MODERN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_is_virt_in_little_endian() {
        assert_eq!(MAGIC.to_le_bytes(), *b"virt");
    }

    #[test]
    fn only_a_modern_slot_is_accepted() {
        assert!(is_modern(MAGIC, 2));
        assert!(!is_modern(MAGIC, 1));
        assert!(!is_modern(0, 2));
    }
}
