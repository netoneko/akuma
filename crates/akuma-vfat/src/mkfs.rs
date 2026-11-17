//! Formatting: write a fresh, empty FAT volume.
//!
//! Used by the tests (so they need no host tool) and by a future `mkfs.vfat`
//! userspace command. Always 512-byte sectors and two FATs.

use crate::BlockDevice;
use crate::layout::{FatKind, Geometry};
use akuma_vfs::FsError;

#[derive(Clone, Copy, Debug)]
pub struct FormatOptions {
    /// `None` picks by size: FAT12 up to 16 MiB, FAT16 up to 512 MiB, else
    /// FAT32 — moving up a kind when the smaller one cannot describe the size.
    pub kind: Option<FatKind>,
    pub volume_id: u32,
    /// Space-padded, upper case by convention.
    pub label: [u8; 11],
    /// `None` picks the smallest cluster size the chosen kind allows.
    pub sectors_per_cluster: Option<u32>,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self { kind: None, volume_id: 0x414B_554D, label: *b"NO NAME    ", sectors_per_cluster: None }
    }
}

const SECTOR: u32 = 512;

struct Plan {
    kind: FatKind,
    spc: u32,
    reserved: u32,
    root_entries: u32,
    fat_sectors: u32,
    cluster_count: u32,
}

fn plan_for(total: u32, kind: FatKind, spc: u32) -> Option<Plan> {
    let (reserved, root_entries): (u32, u32) = if kind == FatKind::Fat32 { (32, 0) } else { (1, 512) };
    let root_sectors = (root_entries * 32).div_ceil(SECTOR);
    let mut fat = 1u32;
    loop {
        let overhead = reserved + 2 * fat + root_sectors;
        if overhead >= total {
            return None;
        }
        let clusters = (total - overhead) / spc;
        let need = ((u64::from(clusters) + 2) * u64::from(kind.bits())).div_ceil(8).div_ceil(u64::from(SECTOR));
        let need = u32::try_from(need).ok()?;
        if need <= fat {
            if clusters == 0 || FatKind::from_cluster_count(clusters) != kind {
                return None;
            }
            return Some(Plan { kind, spc, reserved, root_entries, fat_sectors: fat, cluster_count: clusters });
        }
        fat = need;
    }
}

fn choose(total: u32, opts: &FormatOptions) -> Option<Plan> {
    let bytes = u64::from(total) * u64::from(SECTOR);
    let kinds: &[FatKind] = match opts.kind {
        Some(FatKind::Fat12) => &[FatKind::Fat12],
        Some(FatKind::Fat16) => &[FatKind::Fat16],
        Some(FatKind::Fat32) => &[FatKind::Fat32],
        None if bytes <= 16 << 20 => &[FatKind::Fat12, FatKind::Fat16, FatKind::Fat32],
        None if bytes <= 512 << 20 => &[FatKind::Fat16, FatKind::Fat32],
        None => &[FatKind::Fat32],
    };
    for &kind in kinds {
        match opts.sectors_per_cluster {
            Some(spc) => {
                if let Some(p) = plan_for(total, kind, spc) {
                    return Some(p);
                }
            }
            None => {
                for spc in [1, 2, 4, 8, 16, 32, 64] {
                    if let Some(p) = plan_for(total, kind, spc) {
                        return Some(p);
                    }
                }
            }
        }
    }
    None
}

fn zero(dev: &impl BlockDevice, mut off: u64, mut len: u64) -> Result<(), FsError> {
    let buf = [0u8; 4096];
    while len > 0 {
        let n = len.min(4096) as usize;
        dev.write_bytes(off, &buf[..n]).map_err(|()| FsError::IoError)?;
        off += n as u64;
        len -= n as u64;
    }
    Ok(())
}

fn put16(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// Format `total_bytes` of `dev` and return the geometry of the result.
///
/// `NoSpace` if no cluster size makes a valid volume of that kind and size
/// (a FAT32 volume needs at least ~33 MB; a FAT12 one at most ~32 MB).
pub fn format(dev: &impl BlockDevice, total_bytes: u64, opts: &FormatOptions) -> Result<Geometry, FsError> {
    let total = u32::try_from(total_bytes / u64::from(SECTOR)).map_err(|_| FsError::NoSpace)?;
    let p = choose(total, opts).ok_or(FsError::NoSpace)?;
    let fat32 = p.kind == FatKind::Fat32;

    let mut bs = [0u8; 512];
    bs[0..3].copy_from_slice(&[0xEB, if fat32 { 0x58 } else { 0x3C }, 0x90]);
    bs[3..11].copy_from_slice(b"AKUMAFAT");
    put16(&mut bs, 11, SECTOR);
    bs[13] = p.spc as u8;
    put16(&mut bs, 14, p.reserved);
    bs[16] = 2;
    put16(&mut bs, 17, p.root_entries);
    if total < 0x1_0000 && !fat32 {
        put16(&mut bs, 19, total);
    } else {
        put32(&mut bs, 32, total);
    }
    bs[21] = 0xF8;
    if !fat32 {
        put16(&mut bs, 22, p.fat_sectors);
    }
    put16(&mut bs, 24, 63);
    put16(&mut bs, 26, 255);
    if fat32 {
        put32(&mut bs, 36, p.fat_sectors);
        put32(&mut bs, 44, 2);
        put16(&mut bs, 48, 1);
        put16(&mut bs, 50, 6);
        bs[64] = 0x80;
        bs[66] = 0x29;
        put32(&mut bs, 67, opts.volume_id);
        bs[71..82].copy_from_slice(&opts.label);
        bs[82..90].copy_from_slice(b"FAT32   ");
    } else {
        bs[36] = 0x80;
        bs[38] = 0x29;
        put32(&mut bs, 39, opts.volume_id);
        bs[43..54].copy_from_slice(&opts.label);
        bs[54..62].copy_from_slice(if p.kind == FatKind::Fat12 { b"FAT12   " } else { b"FAT16   " });
    }
    bs[510] = 0x55;
    bs[511] = 0xAA;

    let geo = Geometry::parse(&bs)?;
    debug_assert_eq!(geo.kind, p.kind);
    debug_assert_eq!(geo.cluster_count, p.cluster_count);

    let w = |off: u64, d: &[u8]| dev.write_bytes(off, d).map_err(|()| FsError::IoError);
    w(0, &bs)?;

    if fat32 {
        let mut fi = [0u8; 512];
        put32(&mut fi, 0, 0x4161_5252);
        put32(&mut fi, 484, 0x6141_7272);
        put32(&mut fi, 488, geo.cluster_count - 1); // everything but the root cluster
        put32(&mut fi, 492, 3);
        put32(&mut fi, 508, 0xAA55_0000);
        w(u64::from(SECTOR), &fi)?;
        w(6 * u64::from(SECTOR), &bs)?;
        w(7 * u64::from(SECTOR), &fi)?;
        // sectors 2..6 and the rest of the reserved area stay as the device had them,
        // except that nothing may read garbage as a boot-time structure:
        zero(dev, 2 * u64::from(SECTOR), 4 * u64::from(SECTOR))?;
    }

    // FATs: zero, then the two reserved entries (and the root cluster's EOC).
    let mut head = [0u8; 12];
    match p.kind {
        FatKind::Fat12 => head[..3].copy_from_slice(&[0xF8, 0xFF, 0xFF]),
        FatKind::Fat16 => head[..4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF]),
        FatKind::Fat32 => {
            head[..8].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0xFF]);
            head[8..12].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x0F]);
        }
    }
    let used = if fat32 { 12 } else if p.kind == FatKind::Fat12 { 3 } else { 4 };
    for i in 0..u64::from(geo.num_fats) {
        let base = geo.fat_start + i * geo.fat_bytes;
        zero(dev, base, geo.fat_bytes)?;
        w(base, &head[..used])?;
    }

    // Root directory.
    if fat32 {
        zero(dev, geo.cluster_offset(geo.root_cluster), u64::from(geo.cluster_bytes))?;
    } else {
        zero(dev, geo.root_dir_start, geo.root_dir_bytes)?;
    }
    dev.flush().map_err(|()| FsError::IoError)?;
    Ok(geo)
}
