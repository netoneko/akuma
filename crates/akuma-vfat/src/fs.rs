//! The filesystem: a [`Filesystem`] over a [`BlockDevice`].
//!
//! One lock (`Vfat::state`) covers everything, taken with preemption off the
//! same way `akuma-ext2` takes its own. Inside it there is no cache: the FAT is
//! read through the device on demand and a directory is loaded whole when it is
//! looked at. That is a deliberate floor, not an oversight — the volume this is
//! for is a boot partition of a few hundred files — and the kernel's block
//! layer already caches the sectors underneath.
//!
//! Vocabulary used below:
//! - a *dref* names a directory: `0` is the fixed root of FAT12/16, anything
//!   else is the first cluster of the directory (FAT32's root included);
//! - a *slot* is the index of a 32-byte entry within a directory;
//! - an [`Entry`] is a file or directory as found on disk: its name, its short
//!   entry's raw bytes, and which slots it occupies.

use crate::BlockDevice;
use crate::layout::{
    ATTR_ARCHIVE, ATTR_DIRECTORY, ATTR_LONG_NAME, ATTR_READ_ONLY, ATTR_VOLUME_ID, DIR_ENTRY_SIZE, FatKind,
    Geometry, MAX_DIR_BYTES, build_lfn, dos_to_unix, encode_short_exact, lfn_checksum, lfn_units, make_alias,
    names_equal, short_display, unix_to_dos, valid_name,
};
use akuma_primitives::PreemptGuard;
use akuma_vfs::{DirEntry, Filesystem, FsError, FsStats, Metadata};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use spinning_top::Spinlock;

type R<T> = Result<T, FsError>;

// ------------------------------------------------------------------- types

/// A file or directory as it is on disk.
#[derive(Clone)]
struct Entry {
    name: String,
    /// The short entry, verbatim.
    raw: [u8; DIR_ENTRY_SIZE],
    /// First slot of the entry: its first long-name slot, or `slot` if none.
    first_slot: usize,
    /// The short entry's slot.
    slot: usize,
    /// First data cluster (0 = none / the root).
    first: u32,
}

impl Entry {
    fn attr(&self) -> u8 {
        self.raw[11]
    }
    fn is_dir(&self) -> bool {
        self.attr() & ATTR_DIRECTORY != 0
    }
    fn is_ro(&self) -> bool {
        self.attr() & ATTR_READ_ONLY != 0
    }
    fn size(&self) -> u32 {
        u32::from_le_bytes([self.raw[28], self.raw[29], self.raw[30], self.raw[31]])
    }
    fn matches(&self, name: &str) -> bool {
        names_equal(&self.name, name) || names_equal(&short_display(self.short(), self.raw[12]), name)
    }
    fn short(&self) -> &[u8; 11] {
        self.raw[..11].try_into().unwrap_or(&[b' '; 11])
    }
}

/// A directory read into memory, with the clusters it came from.
struct DirImage {
    dref: u32,
    bytes: Vec<u8>,
    clusters: Vec<u32>,
}

/// Where a path ended up.
#[derive(Clone)]
enum Node {
    Root,
    Ent { parent: u32, ent: Entry },
}

enum Fat {
    Free,
    Next(u32),
    Bad,
    End,
}

struct State<D: BlockDevice> {
    dev: D,
    g: Geometry,
    now: fn() -> u64,
    /// Allocation hint: where the next free-cluster search starts.
    next_free: u32,
    /// The FSInfo hints have been invalidated since mount (done once, on the
    /// first write, so a crash cannot leave a stale count that is trusted).
    fsinfo_invalid: bool,
}

/// Dropping the last handle (an unmount) is the volume's last chance to leave
/// FSInfo truthful and the device flushed; `umount(2)` does not call `sync`.
impl<D: BlockDevice> Drop for State<D> {
    fn drop(&mut self) {
        let _ = self.refresh_fsinfo();
        let _ = self.dev.flush();
    }
}

/// A mounted FAT volume.
pub struct Vfat<D: BlockDevice> {
    state: Spinlock<State<D>>,
}

impl<D: BlockDevice> Vfat<D> {
    /// Mount the volume on `dev`. `now` is the wall clock in Unix seconds, used
    /// for the timestamps of everything this mount creates or modifies.
    pub fn mount(dev: D, now: fn() -> u64) -> R<Self> {
        let mut sector = [0u8; 512];
        dev.read_bytes(0, &mut sector).map_err(|()| FsError::IoError)?;
        let g = Geometry::parse(&sector)?;
        Ok(Self { state: Spinlock::new(State { dev, g, now, next_free: 2, fsinfo_invalid: false }) })
    }

    #[must_use]
    pub fn geometry(&self) -> Geometry {
        let _p = PreemptGuard::new();
        self.state.lock().g
    }

    fn with<T>(&self, f: impl FnOnce(&mut State<D>) -> R<T>) -> R<T> {
        let _p = PreemptGuard::new();
        let mut s = self.state.lock();
        f(&mut s)
    }
}

fn le16(b: &[u8], o: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[o], b[o + 1]]))
}

fn split_parent(path: &str) -> R<(&str, &str)> {
    let p = path.trim_end_matches('/');
    let (dir, leaf) = match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p),
    };
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        return Err(FsError::InvalidPath);
    }
    Ok((dir, leaf))
}

// ------------------------------------------------------------- State: I/O

impl<D: BlockDevice> State<D> {
    fn rd(&self, off: u64, buf: &mut [u8]) -> R<()> {
        self.dev.read_bytes(off, buf).map_err(|()| FsError::IoError)
    }

    fn wr(&self, off: u64, data: &[u8]) -> R<()> {
        self.dev.write_bytes(off, data).map_err(|()| FsError::IoError)
    }

    fn root_ref(&self) -> u32 {
        self.g.root_cluster
    }

    /// The directory a cluster number in an entry means (`0` = the root).
    fn dref_of(&self, first: u32) -> R<u32> {
        if first == 0 {
            Ok(self.root_ref())
        } else if self.g.valid_cluster(first) {
            Ok(first)
        } else {
            Err(FsError::Corrupt)
        }
    }

    /// First FAT's byte offset for entry `c`.
    fn fat_off(&self, c: u32) -> u64 {
        let n = u64::from(c);
        self.g.fat_start
            + match self.g.kind {
                FatKind::Fat12 => n + n / 2,
                FatKind::Fat16 => n * 2,
                FatKind::Fat32 => n * 4,
            }
    }

    fn fat_raw(&self, c: u32) -> R<u32> {
        let off = self.fat_off(c);
        match self.g.kind {
            FatKind::Fat12 => {
                let mut b = [0u8; 2];
                self.rd(off, &mut b)?;
                let v = u32::from(u16::from_le_bytes(b));
                Ok(if c & 1 == 1 { v >> 4 } else { v & 0xFFF })
            }
            FatKind::Fat16 => {
                let mut b = [0u8; 2];
                self.rd(off, &mut b)?;
                Ok(u32::from(u16::from_le_bytes(b)))
            }
            FatKind::Fat32 => {
                let mut b = [0u8; 4];
                self.rd(off, &mut b)?;
                Ok(u32::from_le_bytes(b) & 0x0FFF_FFFF)
            }
        }
    }

    fn fat_get(&self, c: u32) -> R<Fat> {
        let v = self.fat_raw(c)?;
        let k = self.g.kind;
        Ok(if v == 0 {
            Fat::Free
        } else if v >= k.eoc_min() {
            Fat::End
        } else if v == k.bad() {
            Fat::Bad
        } else if self.g.valid_cluster(v) {
            Fat::Next(v)
        } else {
            return Err(FsError::Corrupt);
        })
    }

    /// Write FAT entry `c` in every copy of the FAT.
    fn fat_set(&self, c: u32, v: u32) -> R<()> {
        for i in 0..u64::from(self.g.num_fats) {
            let off = self.fat_off(c) + i * self.g.fat_bytes;
            match self.g.kind {
                FatKind::Fat12 => {
                    let mut b = [0u8; 2];
                    self.rd(off, &mut b)?;
                    if c & 1 == 1 {
                        b[0] = (b[0] & 0x0F) | (((v & 0x0F) as u8) << 4);
                        b[1] = ((v >> 4) & 0xFF) as u8;
                    } else {
                        b[0] = (v & 0xFF) as u8;
                        b[1] = (b[1] & 0xF0) | ((v >> 8) & 0x0F) as u8;
                    }
                    self.wr(off, &b)?;
                }
                FatKind::Fat16 => self.wr(off, &(v as u16).to_le_bytes())?,
                FatKind::Fat32 => {
                    // The top four bits are reserved and must be preserved.
                    let mut b = [0u8; 4];
                    self.rd(off, &mut b)?;
                    let old = u32::from_le_bytes(b);
                    self.wr(off, &((old & 0xF000_0000) | (v & 0x0FFF_FFFF)).to_le_bytes())?;
                }
            }
        }
        Ok(())
    }

    /// Visit the raw FAT entries `lo..=hi` of the first FAT until `f` returns
    /// `true`. FAT16/32 are read in 4 KiB windows; FAT12 is small enough to go
    /// entry by entry.
    fn scan(&self, lo: u32, hi: u32, mut f: impl FnMut(u32, u32) -> bool) -> R<()> {
        if lo > hi {
            return Ok(());
        }
        if self.g.kind == FatKind::Fat12 {
            for c in lo..=hi {
                if f(c, self.fat_raw(c)?) {
                    break;
                }
            }
            return Ok(());
        }
        let esz: u64 = if self.g.kind == FatKind::Fat16 { 2 } else { 4 };
        let mut win = [0u8; 4096];
        let (mut win_start, mut win_len) = (0u64, 0u64);
        for c in lo..=hi {
            let e = u64::from(c) * esz;
            if win_len == 0 || e < win_start || e + esz > win_start + win_len {
                win_start = e & !4095;
                win_len = 4096.min(self.g.fat_bytes - win_start);
                self.rd(self.g.fat_start + win_start, &mut win[..win_len as usize])?;
            }
            let o = (e - win_start) as usize;
            let raw = if esz == 2 {
                u32::from(u16::from_le_bytes([win[o], win[o + 1]]))
            } else {
                u32::from_le_bytes([win[o], win[o + 1], win[o + 2], win[o + 3]]) & 0x0FFF_FFFF
            };
            if f(c, raw) {
                break;
            }
        }
        Ok(())
    }

    fn count_free(&self) -> R<u32> {
        let mut n = 0;
        self.scan(2, self.g.cluster_count + 1, |_, raw| {
            n += u32::from(raw == 0);
            false
        })?;
        Ok(n)
    }

    fn find_free(&self) -> R<Option<u32>> {
        let last = self.g.cluster_count + 1;
        let start = if self.g.valid_cluster(self.next_free) { self.next_free } else { 2 };
        let mut found = None;
        self.scan(start, last, |c, raw| {
            if raw == 0 {
                found = Some(c);
            }
            raw == 0
        })?;
        if found.is_none() && start > 2 {
            self.scan(2, start - 1, |c, raw| {
                if raw == 0 {
                    found = Some(c);
                }
                raw == 0
            })?;
        }
        Ok(found)
    }

    fn zero_cluster(&self, c: u32) -> R<()> {
        let z = [0u8; 4096];
        let base = self.g.cluster_offset(c);
        let mut done = 0u64;
        while done < u64::from(self.g.cluster_bytes) {
            let n = (u64::from(self.g.cluster_bytes) - done).min(4096) as usize;
            self.wr(base + done, &z[..n])?;
            done += n as u64;
        }
        Ok(())
    }

    /// Take a free cluster, mark it end-of-chain, link it after `prev` (0 = a
    /// new chain) and zero it.
    fn alloc_cluster(&mut self, prev: u32) -> R<u32> {
        let c = self.find_free()?.ok_or(FsError::NoSpace)?;
        self.fat_set(c, self.g.kind.eoc())?;
        if prev != 0 {
            self.fat_set(prev, c)?;
        }
        self.zero_cluster(c)?;
        self.next_free = if c + 1 > self.g.cluster_count + 1 { 2 } else { c + 1 };
        Ok(c)
    }

    /// The cluster after `c`; `None` at end of chain. A free or bad cluster in
    /// the middle of a chain is corruption.
    fn next_in_chain(&self, c: u32) -> R<Option<u32>> {
        match self.fat_get(c)? {
            Fat::Next(n) => Ok(Some(n)),
            Fat::End => Ok(None),
            Fat::Free | Fat::Bad => Err(FsError::Corrupt),
        }
    }

    fn free_chain(&mut self, first: u32) -> R<()> {
        let mut c = first;
        let mut budget = self.g.cluster_count;
        while c != 0 && budget > 0 {
            budget -= 1;
            let next = match self.fat_get(c)? {
                Fat::Next(n) => Some(n),
                Fat::End | Fat::Bad | Fat::Free => None,
            };
            self.fat_set(c, 0)?;
            if c < self.next_free {
                self.next_free = c;
            }
            match next {
                Some(n) => c = n,
                None => break,
            }
        }
        Ok(())
    }

    /// Invalidate FSInfo's free-count and next-free hints, once.
    fn touch_fs(&mut self) -> R<()> {
        if self.fsinfo_invalid {
            return Ok(());
        }
        self.fsinfo_invalid = true;
        if self.g.kind == FatKind::Fat32 && self.g.fsinfo_sector != 0 {
            let base = u64::from(self.g.fsinfo_sector) * u64::from(self.g.bytes_per_sector);
            let mut sig = [0u8; 4];
            self.rd(base + 484, &mut sig)?;
            if u32::from_le_bytes(sig) == 0x6141_7272 {
                self.wr(base + 488, &[0xFF; 8])?;
            }
        }
        Ok(())
    }

    /// Write the real free count and next-free hint into FSInfo, and re-arm
    /// [`Self::touch_fs`] so the next write invalidates them again.
    fn refresh_fsinfo(&mut self) -> R<()> {
        if !self.fsinfo_invalid {
            return Ok(());
        }
        if self.g.kind == FatKind::Fat32 && self.g.fsinfo_sector != 0 {
            let base = u64::from(self.g.fsinfo_sector) * u64::from(self.g.bytes_per_sector);
            let mut sig = [0u8; 4];
            self.rd(base + 484, &mut sig)?;
            if u32::from_le_bytes(sig) == 0x6141_7272 {
                let mut hints = [0u8; 8];
                hints[..4].copy_from_slice(&self.count_free()?.to_le_bytes());
                hints[4..].copy_from_slice(&self.next_free.to_le_bytes());
                self.wr(base + 488, &hints)?;
            }
        }
        self.fsinfo_invalid = false;
        Ok(())
    }

    // ------------------------------------------------------------ file data

    fn read_data(&self, first: u32, size: u32, offset: u64, buf: &mut [u8]) -> R<usize> {
        if first == 0 || offset >= u64::from(size) {
            return Ok(0);
        }
        let n = buf.len().min((u64::from(size) - offset) as usize);
        let cb = u64::from(self.g.cluster_bytes);
        let mut c = first;
        for _ in 0..offset / cb {
            c = self.next_in_chain(c)?.ok_or(FsError::Corrupt)?;
        }
        let (mut done, mut pos) = (0usize, offset);
        while done < n {
            let inoff = pos % cb;
            // Merge physically adjacent clusters into one device read.
            let (mut run_end, mut avail) = (c, cb - inoff);
            while avail < (n - done) as u64 {
                match self.next_in_chain(run_end)? {
                    Some(nx) if nx == run_end + 1 => {
                        run_end = nx;
                        avail += cb;
                    }
                    _ => break,
                }
            }
            let k = avail.min((n - done) as u64) as usize;
            self.rd(self.g.cluster_offset(c) + inoff, &mut buf[done..done + k])?;
            done += k;
            pos += k as u64;
            if done < n {
                c = self.next_in_chain(run_end)?.ok_or(FsError::Corrupt)?;
            }
        }
        Ok(n)
    }

    /// Write `data` at byte `start` of the chain starting at `*first`,
    /// allocating clusters as the chain runs out (and a first one if `*first`
    /// is 0). Never touches the directory entry; the caller does.
    fn write_span(&mut self, first: &mut u32, start: u64, data: &[u8]) -> R<()> {
        if data.is_empty() {
            return Ok(());
        }
        let cb = u64::from(self.g.cluster_bytes);
        let mut c = if *first == 0 {
            let n = self.alloc_cluster(0)?;
            *first = n;
            n
        } else {
            *first
        };
        for _ in 0..start / cb {
            c = match self.next_in_chain(c)? {
                Some(n) => n,
                None => self.alloc_cluster(c)?,
            };
        }
        let (mut pos, mut rest) = (start, data);
        loop {
            let inoff = pos % cb;
            let k = ((cb - inoff) as usize).min(rest.len());
            self.wr(self.g.cluster_offset(c) + inoff, &rest[..k])?;
            pos += k as u64;
            rest = &rest[k..];
            if rest.is_empty() {
                return Ok(());
            }
            c = match self.next_in_chain(c)? {
                Some(n) => n,
                None => self.alloc_cluster(c)?,
            };
        }
    }

    fn fill_zero(&mut self, first: &mut u32, from: u64, to: u64) -> R<()> {
        let z = [0u8; 4096];
        let mut pos = from;
        while pos < to {
            let k = (to - pos).min(4096) as usize;
            self.write_span(first, pos, &z[..k])?;
            pos += k as u64;
        }
        Ok(())
    }

    /// Cut the chain down to what `new_size` bytes need.
    fn shrink(&mut self, first: &mut u32, new_size: u64) -> R<()> {
        if new_size == 0 {
            let f = *first;
            *first = 0;
            return self.free_chain(f);
        }
        if *first == 0 {
            return Ok(());
        }
        let keep = new_size.div_ceil(u64::from(self.g.cluster_bytes));
        let mut c = *first;
        for _ in 1..keep {
            c = match self.next_in_chain(c)? {
                Some(n) => n,
                None => return Ok(()), // chain already shorter than asked
            };
        }
        if let Some(tail) = self.next_in_chain(c)? {
            self.fat_set(c, self.g.kind.eoc())?;
            self.free_chain(tail)?;
        }
        Ok(())
    }

    /// Write at `offset` of a file of `old_size` bytes (zero-filling any gap),
    /// returning the new `(first, size)`. On failure, gives back what it
    /// allocated.
    fn write_file_data(&mut self, first: u32, old_size: u32, offset: u64, data: &[u8]) -> R<(u32, u32)> {
        let end = offset + data.len() as u64;
        if end > u64::from(u32::MAX) {
            return Err(FsError::NoSpace);
        }
        let mut f = first;
        let res = (|| {
            if offset > u64::from(old_size) {
                self.fill_zero(&mut f, u64::from(old_size), offset)?;
            }
            self.write_span(&mut f, offset, data)
        })();
        match res {
            Ok(()) => Ok((f, old_size.max(end as u32))),
            Err(e) => {
                let _ = self.shrink(&mut f, u64::from(old_size));
                Err(e)
            }
        }
    }

    // ------------------------------------------------------------ directories

    fn load_dir(&self, dref: u32) -> R<DirImage> {
        if dref == 0 {
            let mut bytes = vec![0u8; self.g.root_dir_bytes as usize];
            self.rd(self.g.root_dir_start, &mut bytes)?;
            return Ok(DirImage { dref, bytes, clusters: Vec::new() });
        }
        let cb = self.g.cluster_bytes as usize;
        let mut clusters = Vec::new();
        let mut c = dref;
        loop {
            clusters.push(c);
            if clusters.len() * cb > MAX_DIR_BYTES {
                return Err(FsError::Corrupt);
            }
            match self.next_in_chain(c)? {
                Some(n) => c = n,
                None => break,
            }
        }
        let mut bytes = vec![0u8; clusters.len() * cb];
        for (i, &c) in clusters.iter().enumerate() {
            self.rd(self.g.cluster_offset(c), &mut bytes[i * cb..(i + 1) * cb])?;
        }
        Ok(DirImage { dref, bytes, clusters })
    }

    /// Device offset and bytes-to-end-of-extent of byte `p` of a directory.
    fn dir_pos(&self, dref: u32, clusters: &[u32], p: usize) -> R<(u64, usize)> {
        if dref == 0 {
            let room = (self.g.root_dir_bytes as usize).checked_sub(p).ok_or(FsError::Corrupt)?;
            return Ok((self.g.root_dir_start + p as u64, room));
        }
        let cb = self.g.cluster_bytes as usize;
        let c = *clusters.get(p / cb).ok_or(FsError::Corrupt)?;
        Ok((self.g.cluster_offset(c) + (p % cb) as u64, cb - p % cb))
    }

    fn flush_dir(&self, img: &DirImage, start: usize, end: usize) -> R<()> {
        let mut p = start;
        while p < end {
            let (off, room) = self.dir_pos(img.dref, &img.clusters, p)?;
            let k = room.min(end - p);
            self.wr(off, &img.bytes[p..p + k])?;
            p += k;
        }
        Ok(())
    }

    /// Device offset of slot `slot` of directory `dref`, walking the chain.
    fn slot_offset(&self, dref: u32, slot: usize) -> R<u64> {
        let byte = slot * DIR_ENTRY_SIZE;
        if dref == 0 {
            if byte >= self.g.root_dir_bytes as usize {
                return Err(FsError::Corrupt);
            }
            return Ok(self.g.root_dir_start + byte as u64);
        }
        let cb = self.g.cluster_bytes as usize;
        let mut c = dref;
        for _ in 0..byte / cb {
            c = self.next_in_chain(c)?.ok_or(FsError::Corrupt)?;
        }
        Ok(self.g.cluster_offset(c) + (byte % cb) as u64)
    }

    fn patch_entry(&self, dref: u32, slot: usize, f: impl FnOnce(&mut [u8; DIR_ENTRY_SIZE])) -> R<()> {
        let off = self.slot_offset(dref, slot)?;
        let mut raw = [0u8; DIR_ENTRY_SIZE];
        self.rd(off, &mut raw)?;
        f(&mut raw);
        self.wr(off, &raw)
    }

    fn delete_entry(&self, dref: u32, e: &Entry) -> R<()> {
        for s in e.first_slot..=e.slot {
            self.patch_entry(dref, s, |raw| raw[0] = 0xE5)?;
        }
        Ok(())
    }

    /// Every file and directory in `img`, long names assembled and checked
    /// against the short entry's checksum; `.`/`..`, volume labels and deleted
    /// slots are skipped. A long-name run that does not match its short entry is
    /// ignored (the entry then shows under its short name), as it must be: an
    /// old DOS rename leaves exactly that.
    fn parse_dir(&self, img: &DirImage) -> Vec<Entry> {
        let mut out = Vec::new();
        let mut parts: Vec<[u16; 13]> = Vec::new();
        let (mut expect, mut sum, mut lfn_first) = (0u8, 0u8, 0usize);
        for i in 0..img.bytes.len() / DIR_ENTRY_SIZE {
            let Ok(raw): Result<[u8; DIR_ENTRY_SIZE], _> =
                img.bytes[i * DIR_ENTRY_SIZE..(i + 1) * DIR_ENTRY_SIZE].try_into()
            else {
                break;
            };
            if raw[0] == 0x00 {
                break;
            }
            if raw[0] == 0xE5 {
                parts.clear();
                expect = 0;
                continue;
            }
            if raw[11] & 0x3F == ATTR_LONG_NAME {
                let ord = raw[0];
                if ord & 0x40 != 0 {
                    parts.clear();
                    expect = ord & 0x3F;
                    sum = raw[13];
                    lfn_first = i;
                } else if expect == 0 || ord != expect || raw[13] != sum {
                    parts.clear();
                    expect = 0;
                    continue;
                }
                if ord & 0x3F != expect || expect == 0 {
                    parts.clear();
                    expect = 0;
                    continue;
                }
                parts.push(lfn_units(&raw));
                expect -= 1;
                continue;
            }
            let short: [u8; 11] = raw[..11].try_into().unwrap_or([b' '; 11]);
            let have_long = expect == 0 && !parts.is_empty() && lfn_checksum(&short) == sum;
            let lo = le16(&raw, 26);
            let hi = if self.g.kind == FatKind::Fat32 { le16(&raw, 20) } else { 0 };
            let entry_first = (hi << 16) | lo;
            let skip = raw[11] & ATTR_VOLUME_ID != 0;
            if !skip {
                let name = if have_long {
                    let mut units: Vec<u16> = Vec::with_capacity(parts.len() * 13);
                    for p in parts.iter().rev() {
                        units.extend_from_slice(p);
                    }
                    let end = units.iter().position(|&u| u == 0 || u == 0xFFFF).unwrap_or(units.len());
                    char::decode_utf16(units[..end].iter().copied())
                        .map(|r| r.unwrap_or('\u{FFFD}'))
                        .collect()
                } else {
                    short_display(&short, raw[12])
                };
                if name != "." && name != ".." {
                    out.push(Entry {
                        name,
                        raw,
                        first_slot: if have_long { lfn_first } else { i },
                        slot: i,
                        first: entry_first,
                    });
                }
            }
            parts.clear();
            expect = 0;
        }
        out
    }

    fn lookup(&self, dref: u32, name: &str) -> R<Option<Entry>> {
        let img = self.load_dir(dref)?;
        Ok(self.parse_dir(&img).into_iter().find(|e| e.matches(name)))
    }

    /// Add `name` to directory `dref`. Fails `AlreadyExists` if the name (long
    /// or as someone's 8.3 alias) is taken.
    fn create_entry(&mut self, dref: u32, name: &str, attr: u8, first: u32, size: u32) -> R<Entry> {
        if !valid_name(name) {
            return Err(FsError::InvalidPath);
        }
        let mut img = self.load_dir(dref)?;
        let existing = self.parse_dir(&img);
        if existing.iter().any(|e| e.matches(name)) {
            return Err(FsError::AlreadyExists);
        }
        let taken = |s: &[u8; 11]| existing.iter().any(|e| e.short() == s);
        let (short, nt, lfn) = if let Some((s, nt)) = encode_short_exact(name) {
            if taken(&s) {
                return Err(FsError::AlreadyExists);
            }
            (s, nt, Vec::new())
        } else {
            let mut n = 1;
            let s = loop {
                let cand = make_alias(name, n);
                if !taken(&cand) {
                    break cand;
                }
                n += 1;
                if n > 999_999 {
                    return Err(FsError::NoSpace);
                }
            };
            (s, 0, build_lfn(name, &s))
        };
        let need = lfn.len() + 1;

        // Find `need` consecutive free slots (deleted, or past the end marker).
        let total = img.bytes.len() / DIR_ENTRY_SIZE;
        let (mut run, mut run_start, mut found) = (0usize, 0usize, None);
        for i in 0..total {
            let b = img.bytes[i * DIR_ENTRY_SIZE];
            if b == 0x00 || b == 0xE5 {
                if run == 0 {
                    run_start = i;
                }
                run += 1;
                if run == need {
                    found = Some(run_start);
                    break;
                }
            } else {
                run = 0;
            }
        }
        let start = if let Some(s) = found { s } else {
            // Grow the directory. The fixed FAT12/16 root cannot.
            if dref == 0 {
                return Err(FsError::NoSpace);
            }
            let cb = self.g.cluster_bytes as usize;
            let more = (need - run) * DIR_ENTRY_SIZE;
            let add = more.div_ceil(cb);
            if img.bytes.len() + add * cb > MAX_DIR_BYTES {
                return Err(FsError::NoSpace);
            }
            let mut prev = *img.clusters.last().ok_or(FsError::Corrupt)?;
            for _ in 0..add {
                let c = self.alloc_cluster(prev)?;
                img.clusters.push(c);
                img.bytes.resize(img.bytes.len() + cb, 0);
                prev = c;
            }
            total - run
        };

        let (date, time, tenths) = unix_to_dos((self.now)());
        let mut raw = [0u8; DIR_ENTRY_SIZE];
        raw[..11].copy_from_slice(&short);
        raw[11] = attr;
        raw[12] = nt;
        raw[13] = tenths;
        raw[14..16].copy_from_slice(&time.to_le_bytes());
        raw[16..18].copy_from_slice(&date.to_le_bytes());
        raw[18..20].copy_from_slice(&date.to_le_bytes());
        if self.g.kind == FatKind::Fat32 {
            raw[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
        }
        raw[22..24].copy_from_slice(&time.to_le_bytes());
        raw[24..26].copy_from_slice(&date.to_le_bytes());
        raw[26..28].copy_from_slice(&((first & 0xFFFF) as u16).to_le_bytes());
        raw[28..32].copy_from_slice(&size.to_le_bytes());

        let mut at = start * DIR_ENTRY_SIZE;
        for l in &lfn {
            img.bytes[at..at + DIR_ENTRY_SIZE].copy_from_slice(l);
            at += DIR_ENTRY_SIZE;
        }
        img.bytes[at..at + DIR_ENTRY_SIZE].copy_from_slice(&raw);
        self.flush_dir(&img, start * DIR_ENTRY_SIZE, at + DIR_ENTRY_SIZE)?;
        Ok(Entry { name: String::from(name), raw, first_slot: start, slot: start + lfn.len(), first })
    }

    // ----------------------------------------------------------------- paths

    fn resolve(&self, path: &str) -> R<Node> {
        let mut cur = Node::Root;
        for comp in path.split('/') {
            if comp.is_empty() || comp == "." {
                continue;
            }
            if comp == ".." {
                return Err(FsError::InvalidPath);
            }
            let dref = self.node_dref(&cur)?;
            match self.lookup(dref, comp)? {
                Some(ent) => cur = Node::Ent { parent: dref, ent },
                None => return Err(FsError::NotFound),
            }
        }
        Ok(cur)
    }

    fn node_dref(&self, n: &Node) -> R<u32> {
        match n {
            Node::Root => Ok(self.root_ref()),
            Node::Ent { ent, .. } if ent.is_dir() => self.dref_of(ent.first),
            Node::Ent { .. } => Err(FsError::NotADirectory),
        }
    }

    /// Resolve `path`'s parent directory and look the leaf up in it.
    fn resolve_leaf<'a>(&self, path: &'a str) -> R<(u32, &'a str, Option<Entry>)> {
        let (pp, leaf) = split_parent(path)?;
        let pdref = self.node_dref(&self.resolve(pp)?)?;
        let ent = self.lookup(pdref, leaf)?;
        Ok((pdref, leaf, ent))
    }

    fn file_node(&self, path: &str) -> R<(u32, Entry)> {
        match self.resolve(path)? {
            Node::Root => Err(FsError::NotAFile),
            Node::Ent { ent, .. } if ent.is_dir() => Err(FsError::NotAFile),
            Node::Ent { parent, ent } => Ok((parent, ent)),
        }
    }

    #[allow(clippy::unused_self)]
    fn metadata_of(&self, n: &Node) -> Metadata {
        match n {
            Node::Root => Metadata {
                is_dir: true,
                size: 0,
                inode: 1,
                mode: 0o040_755,
                created: None,
                modified: None,
                accessed: None,
                links: None,
                uid: 0,
                gid: 0,
            },
            Node::Ent { parent, ent } => {
                let raw = &ent.raw;
                let t = |d: usize, t: usize| {
                    dos_to_unix(u16::from_le_bytes([raw[d], raw[d + 1]]), u16::from_le_bytes([raw[t], raw[t + 1]]))
                };
                let dir = ent.is_dir();
                let perm = match (dir, ent.is_ro()) {
                    (true, false) => 0o755,
                    (true, true) => 0o555,
                    (false, false) => 0o644,
                    (false, true) => 0o444,
                };
                Metadata {
                    is_dir: dir,
                    size: if dir { 0 } else { u64::from(ent.size()) },
                    inode: (u64::from(*parent) + 1) << 32 | ent.slot as u64,
                    mode: if dir { 0o040_000 } else { 0o100_000 } | perm,
                    created: t(16, 14),
                    modified: t(24, 22),
                    accessed: dos_to_unix(u16::from_le_bytes([raw[18], raw[19]]), 0),
                    links: None,
                    uid: 0,
                    gid: 0,
                }
            }
        }
    }

    /// Persist a changed `(first, size)` and stamp the modification time.
    fn commit_file(&self, parent: u32, e: &Entry, first: u32, size: u32) -> R<()> {
        let (date, time, _) = unix_to_dos((self.now)());
        let fat32 = self.g.kind == FatKind::Fat32;
        self.patch_entry(parent, e.slot, |raw| {
            raw[11] |= ATTR_ARCHIVE;
            raw[18..20].copy_from_slice(&date.to_le_bytes());
            raw[20..22].copy_from_slice(&(if fat32 { (first >> 16) as u16 } else { 0 }).to_le_bytes());
            raw[22..24].copy_from_slice(&time.to_le_bytes());
            raw[24..26].copy_from_slice(&date.to_le_bytes());
            raw[26..28].copy_from_slice(&((first & 0xFFFF) as u16).to_le_bytes());
            raw[28..32].copy_from_slice(&size.to_le_bytes());
        })
    }

    // -------------------------------------------------- Filesystem operations

    fn op_read_dir(&self, path: &str) -> R<Vec<DirEntry>> {
        let n = self.resolve(path)?;
        let dref = self.node_dref(&n)?;
        let img = self.load_dir(dref)?;
        Ok(self
            .parse_dir(&img)
            .into_iter()
            .map(|e| DirEntry {
                is_dir: e.is_dir(),
                is_symlink: false,
                size: if e.is_dir() { 0 } else { u64::from(e.size()) },
                name: e.name,
            })
            .collect())
    }

    fn op_read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> R<usize> {
        let (_, e) = self.file_node(path)?;
        self.read_data(e.first, e.size(), offset as u64, buf)
    }

    fn op_read_file(&self, path: &str) -> R<Vec<u8>> {
        let (_, e) = self.file_node(path)?;
        let mut v = vec![0u8; e.size() as usize];
        let n = self.read_data(e.first, e.size(), 0, &mut v)?;
        v.truncate(n);
        Ok(v)
    }

    /// The file at `path`, created empty if absent.
    fn file_or_create(&mut self, path: &str) -> R<(u32, Entry)> {
        let (pdref, leaf, ent) = self.resolve_leaf(path)?;
        match ent {
            Some(e) if e.is_dir() => Err(FsError::NotAFile),
            Some(e) => Ok((pdref, e)),
            None => Ok((pdref, self.create_entry(pdref, leaf, ATTR_ARCHIVE, 0, 0)?)),
        }
    }

    fn op_write_file(&mut self, path: &str, data: &[u8]) -> R<()> {
        if data.len() as u64 > u64::from(u32::MAX) {
            return Err(FsError::NoSpace);
        }
        self.touch_fs()?;
        let (pdref, e) = self.file_or_create(path)?;
        if e.is_ro() {
            return Err(FsError::PermissionDenied);
        }
        // Rewrite over the existing chain, then cut what is left over.
        let (mut first, _) = self.write_file_data(e.first, e.size(), 0, data)?;
        if u64::from(e.size()) > data.len() as u64 || data.is_empty() {
            self.shrink(&mut first, data.len() as u64)?;
        }
        self.commit_file(pdref, &e, first, data.len() as u32)
    }

    fn op_write_at(&mut self, path: &str, offset: usize, data: &[u8]) -> R<usize> {
        self.touch_fs()?;
        let (pdref, e) = self.file_or_create(path)?;
        if e.is_ro() {
            return Err(FsError::PermissionDenied);
        }
        let (first, size) = self.write_file_data(e.first, e.size(), offset as u64, data)?;
        self.commit_file(pdref, &e, first, size)?;
        Ok(data.len())
    }

    fn op_truncate(&mut self, path: &str, len: u64) -> R<()> {
        if len > u64::from(u32::MAX) {
            return Err(FsError::NoSpace);
        }
        self.touch_fs()?;
        let (pdref, e) = self.file_node(path)?;
        if e.is_ro() {
            return Err(FsError::PermissionDenied);
        }
        let mut first = e.first;
        if len < u64::from(e.size()) {
            self.shrink(&mut first, len)?;
        } else if len > u64::from(e.size())
            && let Err(err) = self.fill_zero(&mut first, u64::from(e.size()), len) {
                let _ = self.shrink(&mut first, u64::from(e.size()));
                return Err(err);
            }
        self.commit_file(pdref, &e, first, len as u32)
    }

    fn op_create_dir(&mut self, path: &str) -> R<()> {
        self.touch_fs()?;
        let (pdref, leaf, ent) = self.resolve_leaf(path)?;
        if ent.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let c = self.alloc_cluster(0)?;
        let made = (|| {
            let (date, time, _) = unix_to_dos((self.now)());
            let up = if pdref == self.root_ref() { 0 } else { pdref };
            let mut buf = [0u8; 2 * DIR_ENTRY_SIZE];
            for (i, (name, target)) in [(b".          ", c), (b"..         ", up)].into_iter().enumerate() {
                let raw = &mut buf[i * DIR_ENTRY_SIZE..(i + 1) * DIR_ENTRY_SIZE];
                raw[..11].copy_from_slice(name);
                raw[11] = ATTR_DIRECTORY;
                raw[14..16].copy_from_slice(&time.to_le_bytes());
                raw[16..18].copy_from_slice(&date.to_le_bytes());
                raw[18..20].copy_from_slice(&date.to_le_bytes());
                if self.g.kind == FatKind::Fat32 {
                    raw[20..22].copy_from_slice(&((target >> 16) as u16).to_le_bytes());
                }
                raw[22..24].copy_from_slice(&time.to_le_bytes());
                raw[24..26].copy_from_slice(&date.to_le_bytes());
                raw[26..28].copy_from_slice(&((target & 0xFFFF) as u16).to_le_bytes());
            }
            self.wr(self.g.cluster_offset(c), &buf)?;
            self.create_entry(pdref, leaf, ATTR_DIRECTORY, c, 0)
        })();
        match made {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = self.free_chain(c);
                Err(e)
            }
        }
    }

    fn op_remove_file(&mut self, path: &str) -> R<()> {
        self.touch_fs()?;
        let (pdref, e) = self.file_node(path)?;
        if e.is_ro() {
            return Err(FsError::PermissionDenied);
        }
        self.delete_entry(pdref, &e)?;
        self.free_chain(e.first)
    }

    fn dir_is_empty(&self, e: &Entry) -> R<bool> {
        let img = self.load_dir(self.dref_of(e.first)?)?;
        Ok(self.parse_dir(&img).is_empty())
    }

    fn op_remove_dir(&mut self, path: &str) -> R<()> {
        self.touch_fs()?;
        let (pdref, e) = match self.resolve(path)? {
            Node::Root => return Err(FsError::PermissionDenied),
            Node::Ent { ent, .. } if !ent.is_dir() => return Err(FsError::NotADirectory),
            Node::Ent { parent, ent } => (parent, ent),
        };
        if e.is_ro() {
            return Err(FsError::PermissionDenied);
        }
        if !self.dir_is_empty(&e)? {
            return Err(FsError::DirectoryNotEmpty);
        }
        self.delete_entry(pdref, &e)?;
        self.free_chain(e.first)
    }

    fn op_rename(&mut self, old: &str, new: &str) -> R<()> {
        self.touch_fs()?;
        let (sp, se) = match self.resolve(old)? {
            Node::Root => return Err(FsError::InvalidPath),
            Node::Ent { parent, ent } => (parent, ent),
        };
        let (dpp, dleaf) = split_parent(new)?;
        let ddref = self.node_dref(&self.resolve(dpp)?)?;
        if !valid_name(dleaf) {
            return Err(FsError::InvalidPath);
        }
        if se.is_dir() {
            // A directory cannot move into itself.
            let (o, n) = (old.trim_matches('/'), new.trim_matches('/'));
            let lower = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
            let (o, n) = (lower(o), lower(n));
            if n.len() > o.len() && n.starts_with(&o) && n.as_bytes()[o.len()] == b'/' {
                return Err(FsError::InvalidPath);
            }
        }

        let mut same = false;
        if let Some(de) = self.lookup(ddref, dleaf)? {
            if ddref == sp && de.slot == se.slot {
                if se.name == dleaf {
                    return Ok(());
                }
                same = true; // a change of case only
            } else {
                if de.is_ro() {
                    return Err(FsError::PermissionDenied);
                }
                match (se.is_dir(), de.is_dir()) {
                    (false, true) => return Err(FsError::NotAFile),
                    (true, false) => return Err(FsError::NotADirectory),
                    (true, true) if !self.dir_is_empty(&de)? => return Err(FsError::DirectoryNotEmpty),
                    _ => {}
                }
                self.delete_entry(ddref, &de)?;
                self.free_chain(de.first)?;
            }
        }

        let size = if se.is_dir() { 0 } else { se.size() };
        if same {
            self.delete_entry(sp, &se)?;
        }
        let ne = self.create_entry(ddref, dleaf, se.attr(), se.first, size)?;
        // Keep the old timestamps and the hidden/system bits.
        let old_raw = se.raw;
        self.patch_entry(ddref, ne.slot, |raw| {
            raw[13..20].copy_from_slice(&old_raw[13..20]);
            raw[22..26].copy_from_slice(&old_raw[22..26]);
        })?;
        if !same {
            self.delete_entry(sp, &se)?;
        }
        if se.is_dir() && sp != ddref {
            // Its `..` entry (slot 1) names the new parent.
            let up = if ddref == self.root_ref() { 0 } else { ddref };
            let fat32 = self.g.kind == FatKind::Fat32;
            self.patch_entry(self.dref_of(se.first)?, 1, |raw| {
                raw[20..22].copy_from_slice(&(if fat32 { (up >> 16) as u16 } else { 0 }).to_le_bytes());
                raw[26..28].copy_from_slice(&((up & 0xFFFF) as u16).to_le_bytes());
            })?;
        }
        Ok(())
    }

    fn op_set_times(&mut self, path: &str, atime: Option<u64>, mtime: Option<u64>) -> R<()> {
        self.touch_fs()?;
        let (parent, ent) = match self.resolve(path)? {
            Node::Root => return Err(FsError::NotSupported),
            Node::Ent { parent, ent } => (parent, ent),
        };
        self.patch_entry(parent, ent.slot, |raw| {
            if let Some(a) = atime {
                raw[18..20].copy_from_slice(&unix_to_dos(a).0.to_le_bytes());
            }
            if let Some(m) = mtime {
                let (d, t, _) = unix_to_dos(m);
                raw[22..24].copy_from_slice(&t.to_le_bytes());
                raw[24..26].copy_from_slice(&d.to_le_bytes());
            }
        })
    }

    fn op_chmod(&mut self, path: &str, mode: u32) -> R<()> {
        self.touch_fs()?;
        let (parent, ent) = match self.resolve(path)? {
            Node::Root => return Ok(()),
            Node::Ent { parent, ent } => (parent, ent),
        };
        self.patch_entry(parent, ent.slot, |raw| {
            if mode & 0o222 == 0 {
                raw[11] |= ATTR_READ_ONLY;
            } else {
                raw[11] &= !ATTR_READ_ONLY;
            }
        })
    }
}

// ------------------------------------------------------------ the trait impl

impl<D: BlockDevice> Filesystem for Vfat<D> {
    #[allow(clippy::unnecessary_literal_bound)] // the trait fixes the signature
    fn name(&self) -> &str {
        "vfat"
    }

    fn read_dir(&self, path: &str) -> R<Vec<DirEntry>> {
        self.with(|s| s.op_read_dir(path))
    }

    fn read_file(&self, path: &str) -> R<Vec<u8>> {
        self.with(|s| s.op_read_file(path))
    }

    fn write_file(&self, path: &str, data: &[u8]) -> R<()> {
        self.with(|s| s.op_write_file(path, data))
    }

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> R<usize> {
        self.with(|s| s.op_read_at(path, offset, buf))
    }

    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> R<usize> {
        self.with(|s| s.op_write_at(path, offset, data))
    }

    /// Size read and write happen under one hold of the lock.
    fn append(&self, path: &str, data: &[u8]) -> R<(usize, usize)> {
        self.with(|s| {
            let (_, e) = s.file_or_create(path)?;
            let off = e.size() as usize;
            let n = s.op_write_at(path, off, data)?;
            Ok((off, n))
        })
    }

    fn create_dir(&self, path: &str) -> R<()> {
        self.with(|s| s.op_create_dir(path))
    }

    fn remove_file(&self, path: &str) -> R<()> {
        self.with(|s| s.op_remove_file(path))
    }

    fn remove_dir(&self, path: &str) -> R<()> {
        self.with(|s| s.op_remove_dir(path))
    }

    fn exists(&self, path: &str) -> bool {
        self.with(|s| s.resolve(path).map(|_| ())).is_ok()
    }

    fn metadata(&self, path: &str) -> R<Metadata> {
        self.with(|s| Ok(s.metadata_of(&s.resolve(path)?)))
    }

    fn chmod(&self, path: &str, mode: u32) -> R<()> {
        self.with(|s| s.op_chmod(path, mode))
    }

    fn set_times(&self, path: &str, atime_secs: Option<u64>, mtime_secs: Option<u64>) -> R<()> {
        self.with(|s| s.op_set_times(path, atime_secs, mtime_secs))
    }

    fn truncate(&self, path: &str, length: u64) -> R<()> {
        self.with(|s| s.op_truncate(path, length))
    }

    fn rename(&self, old_path: &str, new_path: &str) -> R<()> {
        self.with(|s| s.op_rename(old_path, new_path))
    }

    fn stats(&self) -> R<FsStats> {
        self.with(|s| {
            Ok(FsStats {
                block_size: s.g.cluster_bytes,
                total_blocks: u64::from(s.g.cluster_count),
                free_blocks: u64::from(s.count_free()?),
            })
        })
    }

    /// Flush the device, and put true FSInfo hints back if a write since the
    /// last sync invalidated them (so a cleanly synced volume reads as clean to
    /// `fsck`).
    fn sync(&self) -> R<()> {
        self.with(|s| {
            s.refresh_fsinfo()?;
            s.dev.flush().map_err(|()| FsError::IoError)
        })
    }
}
