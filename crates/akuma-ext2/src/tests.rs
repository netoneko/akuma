extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use crate::BlockDevice;
use crate::Ext2Filesystem;

/// In-memory block device for testing.
struct MemBlockDevice {
    data: spinning_top::Spinlock<Vec<u8>>,
}

impl MemBlockDevice {
    fn new(size: usize) -> Self {
        Self {
            data: spinning_top::Spinlock::new(vec![0u8; size]),
        }
    }
}

impl BlockDevice for MemBlockDevice {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        let data = self.data.lock();
        let off = offset as usize;
        if off + buf.len() > data.len() {
            return Err(());
        }
        buf.copy_from_slice(&data[off..off + buf.len()]);
        Ok(())
    }

    fn write_bytes(&self, offset: u64, buf: &[u8]) -> Result<(), ()> {
        let mut data = self.data.lock();
        let off = offset as usize;
        if off + buf.len() > data.len() {
            return Err(());
        }
        data[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

#[test]
fn block_device_roundtrip() {
    let dev = MemBlockDevice::new(4096);
    dev.write_bytes(100, b"hello").unwrap();
    let mut buf = [0u8; 5];
    dev.read_bytes(100, &mut buf).unwrap();
    assert_eq!(&buf, b"hello");
}

#[test]
fn block_device_out_of_bounds() {
    let dev = MemBlockDevice::new(64);
    assert!(dev.read_bytes(60, &mut [0u8; 10]).is_err());
    assert!(dev.write_bytes(60, &[0u8; 10]).is_err());
}

#[test]
fn mount_zeroed_disk_fails() {
    let dev = MemBlockDevice::new(1024 * 1024);
    let result = Ext2Filesystem::new(dev, || 0);
    assert!(result.is_err(), "zeroed disk should not have valid ext2 magic");
}

#[test]
fn mount_bad_magic_fails() {
    let dev = MemBlockDevice::new(1024 * 1024);
    // Write something at the superblock offset (1024) but not the magic
    dev.write_bytes(1024, &[0xDE, 0xAD]).unwrap();
    let result = Ext2Filesystem::new(dev, || 0);
    assert!(result.is_err());
}
