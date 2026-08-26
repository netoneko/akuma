//! Host-probe support: a counting `BlockDevice`, ext2 geometry parsing, and an
//! [`FsOps`] implementation over `akuma_ext2::Ext2Filesystem`. Compiled only
//! with the `host-probe` feature (which also pulls in `std`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use akuma_ext2::Ext2Filesystem;
use akuma_vfs::Filesystem;

use crate::FsOps;

// ---- ext2 geometry, parsed from the superblock at byte 1024 --------------

/// Just enough of the superblock to bucket a device offset into a region.
#[derive(Clone, Copy)]
pub struct Geom {
    pub block_size: u64,
    pub blocks_per_group: u64,
    pub inodes_per_group: u64,
    inode_size: u64,
    first_data_block: u64,
    pub group_count: u64,
    /// `s_reserved_gdt_blocks` — GDT slots kept free for online resize. Real
    /// `mke2fs` images reserve a long run right after the live GDT, which shifts
    /// the bitmaps and inode table; without accounting for it the region buckets
    /// on `disk.img` mislabel bitmap writes as inode-table writes.
    reserved_gdt_blocks: u64,
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

impl Geom {
    pub fn parse(img: &[u8]) -> Geom {
        let sb = &img[1024..1024 + 264];
        assert_eq!(le16(sb, 56), 0xEF53, "not an ext2 image (bad magic)");
        let total_blocks = le32(sb, 4) as u64;
        let first_data_block = le32(sb, 20) as u64;
        let block_size = 1024u64 << le32(sb, 24);
        let blocks_per_group = le32(sb, 32) as u64;
        let inodes_per_group = le32(sb, 40) as u64;
        let rev = le32(sb, 76);
        let inode_size = if rev >= 1 { le16(sb, 88) as u64 } else { 128 };
        let reserved_gdt_blocks = le16(sb, 0xCE) as u64;
        let group_count = (total_blocks - first_data_block).div_ceil(blocks_per_group);
        Geom {
            block_size,
            blocks_per_group,
            inodes_per_group,
            inode_size,
            first_data_block,
            group_count,
            reserved_gdt_blocks,
        }
    }

    fn region(&self, offset: u64) -> Region {
        if offset == 1024 {
            return Region::Superblock;
        }
        let blk = offset / self.block_size;
        let bpg = self.blocks_per_group;
        let gdt_blocks =
            (self.group_count * 32).div_ceil(self.block_size).max(1) + self.reserved_gdt_blocks;
        let itable_blocks =
            (self.inodes_per_group * self.inode_size).div_ceil(self.block_size);

        for g in 0..self.group_count {
            let start = self.first_data_block + g * bpg;
            let rel = blk.wrapping_sub(start);
            if rel >= bpg {
                continue;
            }
            let sb_here = 1u64; // primary sb at group 0, backup at later groups
            if rel < sb_here {
                return Region::Superblock;
            }
            if rel < sb_here + gdt_blocks {
                return Region::Gdt;
            }
            if rel < sb_here + gdt_blocks + 2 {
                return Region::Bitmap;
            }
            if rel < sb_here + gdt_blocks + 2 + itable_blocks {
                return Region::InodeTable;
            }
            return Region::Data;
        }
        Region::Data
    }
}

#[derive(Clone, Copy)]
enum Region {
    Superblock = 0,
    Gdt = 1,
    Bitmap = 2,
    InodeTable = 3,
    Data = 4,
}

/// Region labels, indexed by `Region as usize`.
pub const REGIONS: [&str; 5] = ["sb", "gdt", "bitmap", "inode_table", "data"];

// ---- counting block device ---------------------------------------------

#[derive(Default)]
pub struct Counters {
    pub r_calls: AtomicU64,
    pub w_calls: AtomicU64,
    pub r_bytes: AtomicU64,
    pub w_bytes: AtomicU64,
    pub w: [AtomicU64; 5],
    pub r: [AtomicU64; 5],
}

impl Counters {
    pub fn snapshot(&self) -> Snap {
        let load = |a: &[AtomicU64; 5]| {
            let mut o = [0u64; 5];
            for i in 0..5 {
                o[i] = a[i].load(Ordering::Relaxed);
            }
            o
        };
        Snap {
            r_calls: self.r_calls.load(Ordering::Relaxed),
            w_calls: self.w_calls.load(Ordering::Relaxed),
            r_bytes: self.r_bytes.load(Ordering::Relaxed),
            w_bytes: self.w_bytes.load(Ordering::Relaxed),
            w: load(&self.w),
            r: load(&self.r),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Snap {
    pub r_calls: u64,
    pub w_calls: u64,
    pub r_bytes: u64,
    pub w_bytes: u64,
    pub w: [u64; 5],
    pub r: [u64; 5],
}

impl std::ops::Sub for Snap {
    type Output = Snap;
    fn sub(self, o: Snap) -> Snap {
        let mut w = [0u64; 5];
        let mut r = [0u64; 5];
        for i in 0..5 {
            w[i] = self.w[i] - o.w[i];
            r[i] = self.r[i] - o.r[i];
        }
        Snap {
            r_calls: self.r_calls - o.r_calls,
            w_calls: self.w_calls - o.w_calls,
            r_bytes: self.r_bytes - o.r_bytes,
            w_bytes: self.w_bytes - o.w_bytes,
            w,
            r,
        }
    }
}

struct CountingDev {
    data: Mutex<Vec<u8>>,
    geom: Geom,
    c: Arc<Counters>,
}

impl akuma_ext2::BlockDevice for CountingDev {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.c.r_calls.fetch_add(1, Ordering::Relaxed);
        self.c.r_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.c.r[self.geom.region(offset) as usize].fetch_add(1, Ordering::Relaxed);
        let data = self.data.lock().unwrap();
        let off = offset as usize;
        if off + buf.len() > data.len() {
            return Err(());
        }
        buf.copy_from_slice(&data[off..off + buf.len()]);
        Ok(())
    }

    fn write_bytes(&self, offset: u64, buf: &[u8]) -> Result<(), ()> {
        self.c.w_calls.fetch_add(1, Ordering::Relaxed);
        self.c.w_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.c.w[self.geom.region(offset) as usize].fetch_add(1, Ordering::Relaxed);
        let mut data = self.data.lock().unwrap();
        let off = offset as usize;
        if off + buf.len() > data.len() {
            return Err(());
        }
        data[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

// ---- FsOps over Ext2Filesystem ----------------------------------------

pub struct HostFsOps {
    fs: Ext2Filesystem<CountingDev>,
    pub counters: Arc<Counters>,
    pub geom: Geom,
}

impl HostFsOps {
    /// Mount `image_bytes` (an in-RAM copy of a real ext2 image).
    pub fn mount(image_bytes: Vec<u8>) -> HostFsOps {
        let geom = Geom::parse(&image_bytes);
        let counters = Arc::new(Counters::default());
        let dev = CountingDev {
            data: Mutex::new(image_bytes),
            geom,
            c: counters.clone(),
        };
        let fs = Ext2Filesystem::new(dev, || 1_700_000_000_000_000).expect("mount ext2");
        HostFsOps { fs, counters, geom }
    }

    pub fn sync(&self) {
        self.fs.sync().expect("sync");
    }

    /// Read `path` back and assert it is exactly `len` bytes, all == `byte`.
    /// Guards against a data-corrupting regression in the zero-fill / write path.
    pub fn verify(&self, path: &str, byte: u8, len: usize) {
        let mut buf = vec![0u8; 8192];
        let mut off = 0;
        loop {
            let n = self.fs.read_at(path, off, &mut buf).expect("verify read");
            if n == 0 {
                break;
            }
            assert!(
                buf[..n].iter().all(|&b| b == byte),
                "verify({path}): byte mismatch at offset {off}"
            );
            off += n;
        }
        assert_eq!(off, len, "verify({path}): length {off}, expected {len}");
    }
}

impl FsOps for HostFsOps {
    fn mkdir(&self, path: &str) {
        match self.fs.create_dir(path) {
            Ok(()) | Err(akuma_vfs::FsError::AlreadyExists) => {}
            Err(e) => panic!("mkdir({path}): {e:?}"),
        }
    }
    fn rmdir(&self, path: &str) {
        self.fs.remove_dir(path).unwrap_or_else(|e| panic!("rmdir({path}): {e:?}"));
    }
    fn create_write(&self, path: &str, data: &[u8]) {
        self.fs.write_file(path, data).unwrap_or_else(|e| panic!("write_file({path}): {e:?}"));
    }
    fn seq_write(&self, path: &str, total: usize, chunk: usize) {
        let buf = vec![0xCDu8; chunk];
        let mut off = 0;
        while off < total {
            let n = core::cmp::min(chunk, total - off);
            let w = self
                .fs
                .write_at(path, off, &buf[..n])
                .unwrap_or_else(|e| panic!("write_at({path}): {e:?}"));
            assert_eq!(w, n, "short write_at({path})");
            off += n;
        }
    }
    fn read_all(&self, path: &str) -> usize {
        let mut buf = vec![0u8; 8192];
        let mut off = 0;
        loop {
            let n = self
                .fs
                .read_at(path, off, &mut buf)
                .unwrap_or_else(|e| panic!("read_at({path}): {e:?}"));
            if n == 0 {
                break;
            }
            off += n;
        }
        off
    }
    fn unlink(&self, path: &str) {
        self.fs.remove_file(path).unwrap_or_else(|e| panic!("unlink({path}): {e:?}"));
    }
    fn list_dir(&self, path: &str) -> usize {
        self.fs.read_dir(path).map(|v| v.len()).unwrap_or(0)
    }
    fn stat(&self, path: &str) {
        self.fs.metadata(path).unwrap_or_else(|e| panic!("stat({path}): {e:?}"));
    }
    fn now_us(&self) -> u64 {
        0
    }
}
