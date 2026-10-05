//! On-demand structural audit — the in-kernel `e2fsck -fn` for the two
//! signatures `docs/archive/AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md` §9/§12
//! chases: a block **claimed by two inodes**, and a block **claimed by an inode
//! but free in its group's bitmap** (the state that hands one file's blocks to
//! the next writer).
//!
//! It exists because the box that shows the bug has no `e2fsck`, no raw
//! device node, and no way back to Ubuntu without a keypress at the TV. It reads
//! through the driver's own cache under the **read** lock, so it sees what the
//! running kernel believes, and it prints every finding through `safe_print!`
//! (readable with `dmesg`). Diagnostic, not hot: it allocates freely (a
//! one-bit-per-block "seen" set and per-group bitmap copies).

use alloc::vec;
use alloc::vec::Vec;

use akuma_primitives::safe_print;
use akuma_vfs::FsError;

use super::{BlockDevice, Ext2Filesystem, Ext2State, Inode, S_IFDIR, S_IFLNK, S_IFREG};

/// What [`Ext2Filesystem::audit`] counted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AuditReport {
    /// Allocated inodes visited.
    pub inodes: u32,
    /// Blocks claimed (data + pointer blocks), counting repeats.
    pub claims: u64,
    /// Claims of a block some earlier claim already named.
    pub cross_linked: u32,
    /// Claims of a block the group bitmap says is free.
    pub claimed_but_free: u32,
    /// Claims outside `[first_data_block, total_blocks)`.
    pub out_of_range: u32,
}

impl AuditReport {
    /// No finding of any kind.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.cross_linked == 0 && self.claimed_but_free == 0 && self.out_of_range == 0
    }
}

/// Cross-link / claimed-but-free findings printed; the counters keep counting
/// past it. Sized so one audit names every damaged file on the box
/// (170 cross-linked blocks on 2026-10-05) without overflowing the 64 KiB
/// dmesg ring: ~2 lines per block at ~90 bytes.
const PRINT_CAP: u32 = 400;
/// Out-of-range claims are a *consequence* (an indirect block that is also
/// another file's data reads as garbage pointers, 2125 of them on the box), so
/// a few lines show the shape and the rest would only evict the boot log.
const OOR_PRINT_CAP: u32 = 32;
/// Cross-linked blocks remembered for the owner-naming second pass.
const DUP_CAP: usize = 1024;

impl<B: BlockDevice> Ext2Filesystem<B> {
    /// Walk every allocated inode and check its block claims. See the module
    /// docs. Prints findings as `[E2-FSCK] ...` and a one-line summary.
    pub fn audit(&self) -> Result<AuditReport, FsError> {
        let state = self.read_state();
        let first = state.first_data_block;
        let total = state.superblock.total_blocks;
        let bpg = state.blocks_per_group;

        let mut seen = vec![0u64; total as usize / 64 + 1];
        let mut bitmaps: Vec<Option<Vec<u8>>> = vec![None; state.block_group_count as usize];
        let mut dups: Vec<u32> = Vec::new();
        let mut rep = AuditReport::default();

        self.audit_walk(&state, &mut rep.inodes, &mut |ino, lb, blk, meta| {
            rep.claims += 1;
            let kind = if meta { "ptr" } else { "data" };
            if blk < first || blk >= total {
                rep.out_of_range += 1;
                if rep.out_of_range <= OOR_PRINT_CAP {
                    safe_print!(160, "[E2-FSCK] inode={} lb={} blk={} ({}): out of range\n", ino, lb.cast_signed(), blk, kind);
                }
                return Ok(());
            }
            let w = blk as usize / 64;
            let m = 1u64 << (blk % 64);
            if seen[w] & m != 0 {
                rep.cross_linked += 1;
                if dups.len() < DUP_CAP && !dups.contains(&blk) {
                    dups.push(blk);
                }
            }
            seen[w] |= m;

            let g = ((blk - first) / bpg) as usize;
            if bitmaps[g].is_none() {
                let bgd = self.read_bgd(&state, g as u32)?;
                bitmaps[g] = Some(self.read_block(&state, bgd.block_bitmap)?);
            }
            let bit = (blk - first) % bpg;
            let set = bitmaps[g]
                .as_deref()
                .is_some_and(|b| b[(bit / 8) as usize] & (1 << (bit % 8)) != 0);
            if !set {
                rep.claimed_but_free += 1;
                if rep.claimed_but_free <= PRINT_CAP {
                    safe_print!(160, "[E2-FSCK] inode={} lb={} blk={} ({}): claimed but FREE in bitmap\n", ino, lb.cast_signed(), blk, kind);
                }
            }
            Ok(())
        })?;

        if !dups.is_empty() {
            // Second pass: name every claimant of each cross-linked block.
            let mut n = 0u32;
            let mut ignore = 0u32;
            self.audit_walk(&state, &mut ignore, &mut |ino, lb, blk, meta| {
                if dups.contains(&blk) {
                    n += 1;
                    if n <= PRINT_CAP * 2 {
                        safe_print!(160, "[E2-FSCK] cross-link blk={} claimed by inode={} lb={} ({})\n",
                            blk, ino, lb.cast_signed(), if meta { "ptr" } else { "data" });
                    }
                }
                Ok(())
            })?;
        }

        safe_print!(192,
            "[E2-FSCK] done: inodes={} claims={} cross_linked={} claimed_but_free={} out_of_range={}\n",
            rep.inodes, rep.claims, rep.cross_linked, rep.claimed_but_free, rep.out_of_range);
        Ok(rep)
    }

    /// Call `f(inode, logical block or u32::MAX, physical block, is_pointer_block)`
    /// for every block claimed by every allocated regular file, directory and
    /// slow symlink. Inodes come from the inode bitmaps, so a sparse inode table
    /// costs nothing.
    fn audit_walk(
        &self,
        st: &Ext2State,
        inodes: &mut u32,
        f: &mut dyn FnMut(u32, u32, u32, bool) -> Result<(), FsError>,
    ) -> Result<(), FsError> {
        let ppb = (st.block_size / 4) as u32;
        for group in 0..st.block_group_count {
            let bgd = self.read_bgd(st, group)?;
            let imap = self.read_block(st, bgd.inode_bitmap)?;
            for bit in 0..st.inodes_per_group {
                if imap[(bit / 8) as usize] & (1 << (bit % 8)) == 0 {
                    continue;
                }
                let ino = group * st.inodes_per_group + bit + 1;
                let inode: Inode = match self.read_inode(st, ino) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                // Reserved inodes (1..=10) other than the root: the resize inode
                // (7) legitimately claims the reserved-GDT blocks the same way
                // across groups, so it would read as one long cross-link.
                if ino < 11 && ino != 2 {
                    continue;
                }
                let ty = inode.type_perms & 0xF000;
                if ty != S_IFREG && ty != S_IFDIR && ty != S_IFLNK {
                    continue;
                }
                if ty == S_IFLNK && inode.sectors_used == 0 {
                    continue; // fast symlink: the "pointers" are the target text
                }
                *inodes += 1;

                // Indexed: `Inode` is packed, so iterating `direct_blocks` would
                // take a reference to an unaligned field (E0793).
                #[allow(clippy::needless_range_loop)]
                for i in 0..12usize {
                    let b = inode.direct_blocks[i];
                    if b != 0 {
                        f(ino, i as u32, b, false)?;
                    }
                }
                let ind = inode.indirect_block;
                if ind != 0 {
                    f(ino, u32::MAX, ind, true)?;
                    let blk = self.read_block(st, ind)?;
                    for j in 0..ppb {
                        let b = Self::read_block_ptr(&blk, j as usize);
                        if b != 0 {
                            f(ino, 12 + j, b, false)?;
                        }
                    }
                }
                let dbl = inode.double_indirect_block;
                if dbl != 0 {
                    f(ino, u32::MAX, dbl, true)?;
                    let d = self.read_block(st, dbl)?;
                    for i in 0..ppb {
                        let ib = Self::read_block_ptr(&d, i as usize);
                        if ib == 0 {
                            continue;
                        }
                        f(ino, u32::MAX, ib, true)?;
                        let blk = self.read_block(st, ib)?;
                        for j in 0..ppb {
                            let b = Self::read_block_ptr(&blk, j as usize);
                            if b != 0 {
                                f(ino, 12 + ppb + i * ppb + j, b, false)?;
                            }
                        }
                    }
                }
                if inode.triple_indirect_block != 0 {
                    f(ino, u32::MAX, inode.triple_indirect_block, true)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{MemBlockDevice, mount_many_inodes};
    use akuma_vfs::Filesystem;

    fn populate(fs: &Ext2Filesystem<MemBlockDevice>) {
        fs.create_dir("/d").unwrap();
        // 1 KiB blocks: 400 KiB reaches into the double-indirect range.
        let big: Vec<u8> = (0..400 * 1024u32).map(|i| (i >> 3) as u8).collect();
        fs.write_file("/d/big", &big).unwrap();
        fs.write_file("/d/small", &[7u8; 5000]).unwrap();
        fs.write_file("/d/other", &[9u8; 3000]).unwrap();
    }

    #[test]
    fn a_healthy_filesystem_audits_clean() {
        let fs = mount_many_inodes();
        populate(&fs);
        let rep = fs.audit().unwrap();
        assert!(rep.is_clean(), "{rep:?}");
        assert!(rep.inodes >= 4 && rep.claims > 400, "{rep:?}");
    }

    #[test]
    fn a_shared_block_and_a_freed_claim_are_both_reported() {
        let fs = mount_many_inodes();
        populate(&fs);
        {
            let mut st = fs.write_state();
            let a_no = fs.lookup_path_internal(&st, "/d/small").unwrap();
            let b_no = fs.lookup_path_internal(&st, "/d/other").unwrap();
            let c_no = fs.lookup_path_internal(&st, "/d/big").unwrap();
            let a = fs.read_inode(&st, a_no).unwrap();
            let mut b = fs.read_inode(&st, b_no).unwrap();
            // Cross-link: `other`'s first block is now `small`'s first block.
            b.direct_blocks[0] = a.direct_blocks[0];
            fs.write_inode(&st, b_no, &b).unwrap();
            // Stale free: `big` still claims a block the bitmap now calls free.
            let c = fs.read_inode(&st, c_no).unwrap();
            fs.free_block(&mut st, c.direct_blocks[3]).unwrap();
            fs.flush_meta(&mut st).unwrap();
        }
        let rep = fs.audit().unwrap();
        assert_eq!(rep.cross_linked, 1, "{rep:?}");
        assert_eq!(rep.claimed_but_free, 1, "{rep:?}");
        assert_eq!(rep.out_of_range, 0, "{rep:?}");
    }
}
