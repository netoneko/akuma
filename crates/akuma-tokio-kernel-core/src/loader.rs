//! The ELF loader.
//!
//! An image (and its `PT_INTERP`) placed into a fresh root, the
//! Linux initial stack built on it, the task's regions and heap windows
//! recorded. Moved from the amd64 half (B21 A5.6) so both machines load the
//! same way; what a machine contributes is its `tree::UserTree` (the root, the
//! leaves) and its `HWCAP` word.
//!
//! The decisions (which segments, where an interpreter or a static-PIE goes) are
//! `akuma-elf-exec`'s, the stack layout is `akuma_elf_exec::stack`'s.

use alloc::string::String;
use alloc::vec::Vec;

use akuma_elf_exec::stack::{self as st, build_stack, stack_block_len};
use akuma_elf_exec::LoadSeg;
use akuma_primitives::safe_print;

use crate::lazy::{self, FilePage, Lazy};
use crate::mm::Mm;
use crate::mmu::FreshPage;
use crate::task::{BRK_SLAB, MMAP_BASE, MMAP_SLAB, USER_STACK_PAGES, USER_STACK_TOP, USER_TOP};
use crate::tree::{self, UserTree};

/// Where a loaded task starts: its root, its entry, its initial stack pointer.
#[derive(Clone, Copy, Debug)]
pub struct TaskImage {
    pub root_pa: u64,
    pub entry: u64,
    pub sp: u64,
}

/// What the loader needs beyond the tree: the capability word the auxv hands
/// the program (`AT_HWCAP`).
pub trait Loader: UserTree {
    /// `AT_HWCAP`.
    const HWCAP: u64 = 0;
}

/// Record `[start, end)` as user memory of the running task.
pub fn add_region<M: Mm>(start: u64, end: u64) {
    if !M::with_task(|t| t.layout.add_region(start, end)) {
        safe_print!(96, "[exec] region table full: [{:#x}, {:#x}) is not user memory\n", start, end);
    }
}

/// A symlink's target on the root (`read_symlink`, which handles the fast
/// inline form), or `None` when `path` is not one.
#[must_use]
pub fn read_link_target<M: lazy::Kernel>(path: &str) -> Option<String> {
    let fs = M::root_fs();
    if !fs.is_symlink(path) {
        return None;
    }
    fs.read_symlink(path).ok().map(|s| String::from(s.trim_end_matches('\n')))
}

/// `path` with its symlinks resolved (8 hops at most).
pub fn resolve_effective<M: lazy::Kernel>(path: &str) -> String {
    akuma_vfs::resolve_symlink_chain(path, 8, read_link_target::<M>)
}

/// A whole file for the loader. `read_file` refuses anything over 16 MiB, so a
/// larger image is read in 1 MiB steps through its inode. One heap block of the
/// file's size either way, allocated fallibly.
pub fn read_image<M: lazy::Kernel>(path: &str) -> Result<Vec<u8>, akuma_vfs::FsError> {
    let fs = M::root_fs();
    let md = fs.metadata(path)?;
    if md.size <= 16 * 1024 * 1024 {
        return fs.read_file(path);
    }
    let ino = fs.resolve_inode(path)?;
    let mut data: Vec<u8> = Vec::new();
    data.try_reserve_exact(md.size as usize).map_err(|_| akuma_vfs::FsError::NoSpace)?;
    data.resize(md.size as usize, 0);
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + (1 << 20)).min(data.len());
        let n = fs.read_at_by_inode(ino, off, &mut data[off..end])?;
        if n == 0 {
            return Err(akuma_vfs::FsError::Internal);
        }
        off += n;
    }
    Ok(data)
}

/// Map one placed `PT_LOAD` into `root`. Returns the page-rounded span, `None`
/// when a frame or a mapping failed or the segment reaches past the user half.
///
/// With `inode` (the image's, on the root) a page that is wholly file bytes is
/// left lazy as a file record (a read-only one fills from the file-page cache,
/// one frame for every process running the image) and a page wholly past the
/// file a zero-fill record. A page the segment's start or file end cuts is
/// filled here. Writable wins over executable.
#[must_use]
pub fn place_segment<M: Loader>(root: u64, image: &[u8], seg: &LoadSeg, inode: Option<u32>) -> Option<(u64, u64)> {
    let write = seg.flags & akuma_elf_exec::PF_W != 0;
    let exec = !write && seg.flags & akuma_elf_exec::PF_X != 0;
    let end = seg.vaddr.checked_add(seg.memsz)?.checked_add(0xFFF)?;
    let (first, last) = (seg.vaddr & !0xFFF, end & !0xFFF);
    if last > USER_TOP {
        return None;
    }
    // A file record names a whole file page: `p_offset` congruent to `p_vaddr`.
    let inode = inode.filter(|&node| seg.offset.wrapping_sub(seg.vaddr).trailing_zeros() >= 12 && lazy::pin(root, node, first, last));
    let file_end = seg.vaddr + seg.filesz;
    let mut pva = first;
    while pva < last {
        let rec = if pva >= seg.vaddr && pva + 4096 <= file_end {
            inode.map(|node| Lazy {
                write,
                exec,
                none: false,
                file: Some(FilePage { inode: node, index: (seg.offset + (pva - seg.vaddr)) / 4096, shared: false }),
            })
        } else if pva >= file_end {
            Some(Lazy::anon(write, exec))
        } else {
            None
        };
        if let Some(rec) = rec.filter(|&rec| lazy::fits(rec, 1)) {
            if !lazy::reserve::<M>(root, pva, 4096, rec) {
                return None;
            }
            pva += 4096;
            continue;
        }
        let mut frame = <M::Frame as FreshPage>::alloc_zeroed()?;
        // The page's overlap with the file-backed range; the slice starts at `p_offset`.
        let from = pva.max(seg.vaddr);
        let to = (pva + 4096).min(file_end);
        if from < to {
            let off = (seg.offset + (from - seg.vaddr)) as usize;
            let at = (from - pva) as usize;
            frame.bytes_mut()[at..at + (to - from) as usize].copy_from_slice(&image[off..off + (to - from) as usize]);
        }
        let pa = frame.pa() as u64;
        if !M::map_page(root, pva, pa, write, exec, true) {
            return None;
        }
        let _ = frame.publish();
        pva += 4096;
    }
    Some((first, last))
}

/// Load an ELF image into the running task's record.
///
/// A fresh root, every `PT_LOAD` placed, the interpreter if any, a Linux initial stack, the heap
/// windows. Returns where to cross. The caller chooses when the root goes live.
#[must_use]
pub fn load_task<M: Loader>(argv: &[&str], env: &[&str], image: &[u8], path: &str) -> Option<TaskImage> {
    const NO_SEG: LoadSeg = LoadSeg { vaddr: 0, offset: 0, filesz: 0, memsz: 0, flags: 0 };
    let mut segs = [NO_SEG; 16];
    let (prog, n) = akuma_elf_exec::program(image, &mut segs).ok()?;
    M::with_task(|t| {
        t.exit = None;
        t.layout.reset_for_exec();
    });
    let root = tree::new_root::<M>()?;
    M::with_task(|t| t.root_pa = root);

    let image_inode = M::root_fs().resolve_inode(path).ok();
    for seg in &segs[..n] {
        let (first, last) = place_segment::<M>(root, image, seg, image_inode)?;
        add_region::<M>(first, last);
    }

    let (cross_entry, at_base) = match akuma_elf_exec::interp_path(image) {
        Ok(Some(ipath)) => {
            let ipath = core::str::from_utf8(ipath).ok()?;
            let eff = resolve_effective::<M>(ipath);
            let Ok(ldso) = read_image::<M>(&eff) else {
                safe_print!(160, "  exec: PT_INTERP unreadable: {}\n", eff.as_str());
                return None;
            };
            let mut isegs = [NO_SEG; 8];
            let Ok(n) = akuma_elf_exec::load_segments(&ldso, akuma_elf_exec::INTERP_BASE, &mut isegs) else {
                safe_print!(96, "  exec: interpreter refused by the exec core\n");
                return None;
            };
            let mut hi = akuma_elf_exec::INTERP_BASE;
            let ldso_inode = M::root_fs().resolve_inode(&eff).ok();
            for seg in &isegs[..n] {
                hi = hi.max(place_segment::<M>(root, &ldso, seg, ldso_inode)?.1);
            }
            add_region::<M>(akuma_elf_exec::INTERP_BASE, hi);
            let Ok(ientry) = akuma_elf_exec::interp_entry(&ldso, akuma_elf_exec::INTERP_BASE) else {
                safe_print!(96, "  exec: interpreter has no entry\n");
                return None;
            };
            (ientry, akuma_elf_exec::INTERP_BASE)
        }
        Ok(None) => (prog.entry, 0),
        Err(_) => return None,
    };

    let aux = [
        (st::AT_BASE, at_base),
        (st::AT_PHDR, prog.phdr_va),
        (st::AT_PHENT, akuma_elf_exec::PHENT),
        (st::AT_PHNUM, prog.phnum),
        (st::AT_PAGESZ, 4096),
        (st::AT_ENTRY, prog.entry),
        (st::AT_UID, 0),
        (st::AT_EUID, 0),
        (st::AT_GID, 0),
        (st::AT_EGID, 0),
        (st::AT_SECURE, 0),
        (st::AT_HWCAP, M::HWCAP),
        (st::AT_CLKTCK, 100),
        (st::AT_RANDOM, 0),
    ];
    // The block is sized by its contents; the stack below it is lazy.
    let block_pages = stack_block_len(argv, env, aux.len()).div_ceil(4096);
    let mut block: Vec<u8> = Vec::new();
    block.try_reserve_exact(block_pages * 4096).ok()?;
    block.resize(block_pages * 4096, 0);
    let sp = build_stack(&mut block, USER_STACK_TOP, argv, env, &aux);

    let stack_pages = (USER_STACK_PAGES as usize).max(block_pages + 4);
    let stack_base = USER_STACK_TOP - stack_pages as u64 * 4096;
    let first_block_page = stack_pages - block_pages;
    if !lazy::reserve::<M>(root, stack_base, first_block_page as u64 * 4096, Lazy::anon(true, false)) {
        return None;
    }
    for p in first_block_page..stack_pages {
        let mut frame = <M::Frame as FreshPage>::alloc_zeroed()?;
        let k = p - first_block_page;
        frame.bytes_mut().copy_from_slice(&block[k * 4096..(k + 1) * 4096]);
        let pva = stack_base + p as u64 * 4096;
        if !M::map_page(root, pva, frame.pa() as u64, true, false, true) {
            return None;
        }
        let _ = frame.publish();
    }
    add_region::<M>(stack_base, USER_STACK_TOP);
    drop(block);

    let brk0 = (prog.end + 0xFFF) & !0xFFF;
    M::with_task(|t| t.layout.set_brk_window(brk0, BRK_SLAB));
    add_region::<M>(brk0, brk0 + BRK_SLAB);
    add_region::<M>(MMAP_BASE, MMAP_BASE + MMAP_SLAB);

    Some(TaskImage { root_pa: root, entry: cross_entry, sp })
}
