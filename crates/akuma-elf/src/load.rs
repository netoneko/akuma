//! Loading an ELF image into a fresh user address space.
//!
//! One implementation, parameterised on two independent axes:
//!
//! * **source** — [`ElfSource`]: bytes already in the heap, or a path read a
//!   piece at a time.
//! * **mapping** — [`MapStrategy`]: allocate and fill every page now, or
//!   register demand-paged lazy regions and let the fault handler do it.
//!
//! `execve` picks the pair by file size (`HEAP_SLURP_MAX` in `process/spawn.rs`):
//! small binaries are slurped and mapped eagerly, large ones stay on disk and
//! are demand-paged.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use elf::abi::{EM_AARCH64, ET_DYN, ET_EXEC, PF_R, PF_W, PF_X, PT_INTERP, PT_LOAD, PT_PHDR};
use elf::segment::ProgramHeader;

use akuma_mmap::{PAGE_SIZE, span, user_flags};
use akuma_mmu::UserAddressSpace;

use super::interp::load_interp_for;
use super::source::{ElfHeaders, ElfSource, parse_headers};
use super::types::{
    DEBUG_ELF_LOADING, DeferredLazySegment, ElfError, FileSegmentSource, InterpInfo,
    R_AARCH64_ABS64, R_AARCH64_GLOB_DAT, R_AARCH64_JUMP_SLOT, R_AARCH64_RELATIVE,
};

/// Static-PIE binaries have `p_vaddr` starting near 0; load them at a fixed base.
const PIE_BASE: usize = 0x1000_0000;

/// Result of loading an ELF binary
pub struct LoadedElf {
    pub entry_point: usize,
    pub address_space: UserAddressSpace,
    pub brk: usize,
    pub phdr_addr: usize,
    pub phnum: usize,
    pub phent: usize,
    pub interp: Option<InterpInfo>,
    pub deferred_segments: Vec<DeferredLazySegment>,
}

/// How PT_LOAD segments reach memory — the *mapping* axis, independent of where
/// the bytes come from.
///
/// `Deferred` carries the path and inode the demand-pager needs, which is why
/// deferring an in-memory image is unrepresentable rather than merely unused:
/// there is nothing to page back in from. That is also why `LoadedElf::
/// deferred_segments` comes back empty from an eager load — it is a property of
/// the strategy, not a special case in the caller.
pub(super) enum MapStrategy<'a> {
    /// Allocate every page now and copy its file-backed bytes in. The only
    /// strategy an in-memory image can use, and the only one that can apply
    /// relocations, which need the target pages to already exist.
    Eager,
    /// Register each PT_LOAD as a lazy region backed by the file; nothing is
    /// read until the process faults on it.
    Deferred { path: &'a str, mount_id: u32, inode: u32 },
}

/// Load an ELF binary from memory.
/// `interp_prefix` is prepended to the PT_INTERP path when loading the dynamic
/// linker (used for container rootfs where the interpreter lives under a prefix).
pub fn load_elf(elf_data: &[u8], interp_prefix: Option<&str>) -> Result<LoadedElf, ElfError> {
    load_image(ElfSource::Bytes(elf_data), &MapStrategy::Eager, interp_prefix)
}

/// Load an ELF binary on demand from a file path, registering each PT_LOAD as a
/// lazy region instead of buffering the file or its segments.
/// Supports PIE (ET_DYN) and non-PIE (ET_EXEC) without relocations — the pages
/// do not exist yet, so there is nothing to relocate.
pub fn load_elf_from_path(
    path: &str,
    file_size: usize,
    interp_prefix: Option<&str>,
) -> Result<LoadedElf, ElfError> {
    let (mount_id, inode) = (crate::vfs().resolve_file_id)(path).unwrap_or((0, 0));

    if DEBUG_ELF_LOADING {
        log::debug!(
            "[ELF] On-demand loading from path, file_size={} ({}MB), inode={}",
            file_size,
            file_size / 1024 / 1024,
            inode
        );
    }

    load_image(
        ElfSource::Path(path),
        &MapStrategy::Deferred { path, mount_id, inode },
        interp_prefix,
    )
}

/// The single ELF load path, shared by every entry point above.
fn load_image(
    src: ElfSource<'_>,
    strategy: &MapStrategy<'_>,
    interp_prefix: Option<&str>,
) -> Result<LoadedElf, ElfError> {
    let headers = parse_headers(src)?;
    let ehdr = headers.ehdr;

    if ehdr.e_machine != EM_AARCH64 {
        return Err(ElfError::WrongArchitecture);
    }

    // Accept ET_EXEC (normal static) and ET_DYN (static-PIE)
    let is_pie = ehdr.e_type == ET_DYN;
    if ehdr.e_type != ET_EXEC && !is_pie {
        return Err(ElfError::NotExecutable);
    }

    let base = if is_pie { PIE_BASE } else { 0 };
    let entry_point = base + ehdr.e_entry as usize;

    // Both strategies walk the segment list more than once; parse it once.
    let phdrs: Vec<ProgramHeader> = headers.segments().iter().collect();
    if phdrs.is_empty() {
        return Err(ElfError::InvalidFormat("No program headers"));
    }

    let mut address_space = UserAddressSpace::new().ok_or(ElfError::AddressSpaceFailed)?;
    let mut brk: usize = 0;
    let mut phdr_addr: usize = 0;

    // Check for PT_INTERP — if present, we need to load the dynamic linker.
    // Static-PIE binaries may have PT_INTERP with an empty string (1-byte null) — skip those.
    let mut interp_path: Option<String> = None;
    for phdr in &phdrs {
        if phdr.p_type == PT_INTERP && phdr.p_filesz > 1 {
            let raw = src.read_at(phdr.p_offset as usize, phdr.p_filesz as usize)?;
            let bytes: &[u8] = raw.as_ref();
            let path_bytes = if bytes.last() == Some(&0) {
                &bytes[..bytes.len() - 1]
            } else {
                bytes
            };
            if let Ok(s) = core::str::from_utf8(path_bytes) {
                interp_path = Some(String::from(s));
            }
        }
    }

    // Find PT_PHDR if it exists
    for phdr in &phdrs {
        if phdr.p_type == PT_PHDR {
            phdr_addr = base + phdr.p_vaddr as usize;
            break;
        }
    }

    // (start_va, file_end_va) per PT_LOAD, used only by the demand-pager to
    // detect boundary pages shared by two segments.
    let pt_loads: Vec<(usize, usize)> = match strategy {
        MapStrategy::Eager => Vec::new(),
        MapStrategy::Deferred { .. } => phdrs
            .iter()
            .filter(|p| p.p_type == PT_LOAD)
            .map(|p| {
                let va = base + p.p_vaddr as usize;
                (va, va + p.p_filesz as usize)
            })
            .collect(),
    };

    // VA -> PA for every page the eager strategy maps. Shared across segments so
    // a page two segments straddle is allocated once, and so the relocation pass
    // can find the frame backing a given VA.
    let mut mapped_pages: BTreeMap<usize, usize> = BTreeMap::new();
    let mut deferred_segments: Vec<DeferredLazySegment> = Vec::new();

    for phdr in &phdrs {
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = base + phdr.p_vaddr as usize;
        let memsz = phdr.p_memsz as usize;

        // Fallback for phdr_addr if PT_PHDR segment was missing
        if phdr_addr == 0 && phdr.p_offset == 0 {
            phdr_addr = vaddr + ehdr.e_phoff as usize;
        }

        match *strategy {
            MapStrategy::Eager => {
                log_segment("Segment", vaddr, phdr.p_filesz as usize, memsz, phdr.p_flags);
                map_segment_eager(src, &mut address_space, base, phdr, &mut mapped_pages)?;
            }
            MapStrategy::Deferred { path, mount_id, inode } => {
                // Boundary-page fix (see `boundary_extended_filesz`): when this
                // segment's file-backed data ends mid-page and the *next* segment
                // begins in that same page, the demand-pager would map the page on
                // this segment's fault, fill only up to `filesz`, and zero the rest —
                // clobbering the next segment's file-backed bytes.
                let filesz = boundary_extended_filesz(vaddr, phdr.p_filesz as usize, &pt_loads);
                log_segment("Segment (deferred)", vaddr, filesz, memsz, phdr.p_flags);

                let start_page = vaddr & !(PAGE_SIZE - 1);
                let end_page = (vaddr + memsz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
                deferred_segments.push(DeferredLazySegment {
                    start_va: start_page,
                    size: end_page - start_page,
                    page_flags: segment_page_flags(phdr.p_flags),
                    file_source: Some(FileSegmentSource {
                        path: String::from(path),
                        mount_id,
                        inode,
                        file_offset: phdr.p_offset as usize,
                        filesz,
                        segment_va: vaddr,
                    }),
                });
            }
        }

        let segment_end = vaddr + memsz;
        if segment_end > brk {
            brk = segment_end;
        }
    }

    match *strategy {
        MapStrategy::Eager => {
            // Apply relocations for ET_EXEC only.
            // Static-PIE (ET_DYN) binaries self-relocate at startup via musl's _dlstart_c.
            // `base` is 0 here — a non-PIE image loads where it was linked.
            if !is_pie {
                apply_relocations(src, &headers, base, &mapped_pages)?;
            }
            if DEBUG_ELF_LOADING {
                log::debug!(
                    "[ELF] Loaded: entry=0x{:x} brk=0x{:x} pages={}",
                    entry_point,
                    brk,
                    mapped_pages.len()
                );
            }
        }
        MapStrategy::Deferred { .. } => {
            push_gap_regions(&phdrs, base, &mut deferred_segments);
            if DEBUG_ELF_LOADING {
                log::debug!(
                    "[ELF] Deferred: entry=0x{:x} brk=0x{:x} segments={}",
                    entry_point,
                    brk,
                    deferred_segments.len()
                );
            }
        }
    }

    let interp = match interp_path {
        Some(ref ipath) => Some(load_interp_for(ipath, interp_prefix, &mut address_space)?),
        None => None,
    };

    Ok(LoadedElf {
        entry_point,
        address_space,
        brk,
        phdr_addr,
        phnum: ehdr.e_phnum as usize,
        phent: ehdr.e_phentsize as usize,
        interp,
        deferred_segments,
    })
}

/// Page permissions for a PT_LOAD segment. Writable segments are never
/// executable and vice versa — W^X, regardless of what `p_flags` asks for.
pub(super) fn segment_page_flags(p_flags: u32) -> u64 {
    if (p_flags & PF_X) != 0 {
        user_flags::RX
    } else {
        user_flags::RW_NO_EXEC
    }
}

fn log_segment(what: &str, vaddr: usize, filesz: usize, memsz: usize, flags: u32) {
    if DEBUG_ELF_LOADING {
        log::debug!(
            "[ELF] {}: VA=0x{:08x} filesz=0x{:x} memsz=0x{:x} flags={}{}{}",
            what,
            vaddr,
            filesz,
            memsz,
            if flags & PF_R != 0 { "R" } else { "-" },
            if flags & PF_W != 0 { "W" } else { "-" },
            if flags & PF_X != 0 { "X" } else { "-" }
        );
    }
}


/// Map one PT_LOAD segment eagerly: allocate each page and copy its file-backed
/// bytes in.
///
/// Shared by the main-binary loader and the interpreter loader, which is why it
/// takes `base` rather than assuming zero. Reads go through `ElfSource`, so the
/// same code serves an in-heap image (borrowed sub-slices, no copy) and a file
/// on disk (one 4 KB-or-less scratch buffer per page, freed immediately).
pub(super) fn map_segment_eager(
    src: ElfSource<'_>,
    address_space: &mut UserAddressSpace,
    base: usize,
    phdr: &ProgramHeader,
    mapped_pages: &mut BTreeMap<usize, usize>,
) -> Result<(), ElfError> {
    let vaddr = base + phdr.p_vaddr as usize;
    let memsz = phdr.p_memsz as usize;
    let filesz = phdr.p_filesz as usize;
    let offset = phdr.p_offset as usize;
    let page_flags = segment_page_flags(phdr.p_flags);

    // Page span and per-page copy window are `akuma_mmap::span`'s, and host-tested
    // there — see that module's header for why arithmetic next to a raw write is
    // the worst place for it.
    let (start_page, num_pages) = span::segment_span(vaddr, memsz);

    for i in 0..num_pages {
        let page_va = start_page + i * PAGE_SIZE;

        let frame_addr = match mapped_pages.get(&page_va) {
            Some(&pa) => pa,
            None => {
                let frame = address_space
                    .alloc_and_map(page_va, page_flags)
                    .map_err(ElfError::MappingFailed)?;
                mapped_pages.insert(page_va, frame.addr);
                frame.addr
            }
        };

        // `None` means the page holds no file bytes: it is past `filesz` and so
        // pure .bss, which `alloc_and_map` already handed us zeroed.
        let Some((dst_offset, src_offset, copy_len)) =
            span::segment_page_copy(page_va, vaddr, filesz)
        else {
            continue;
        };

        let chunk = src.read_at(offset + src_offset, copy_len)?;
        crate::frame_write(frame_addr, dst_offset, &chunk);
    }

    Ok(())
}

/// Apply every SHT_RELA relocation in the image, adding `base` to each computed
/// value. Returns the number applied.
///
/// One implementation for both users: the main binary (ET_EXEC only, `base` 0)
/// and the dynamic linker (`base` = `INTERP_BASE`), which previously had two
/// divergent copies — one going through the `elf` crate, one hand-rolled.
/// Relocations only reach pages present in `mapped_pages`, which is what makes
/// this an eager-mapping-only step.
///
/// A relocation naming symbol 0, or a symbol index outside `.dynsym`, is left
/// alone rather than patched with a bare addend: only ABS64 (`B + A`) has a
/// defined meaning without a symbol. Every ET_EXEC binary in the tree was
/// checked against this — 118 files carrying 2,832 relocations, zero symbol-less
/// ABS64/GLOB_DAT/JUMP_SLOT and zero out-of-range symbol indices — so the rule
/// changes nothing on real input.
pub(super) fn apply_relocations(
    src: ElfSource<'_>,
    headers: &ElfHeaders<'_>,
    base: usize,
    mapped_pages: &BTreeMap<usize, usize>,
) -> Result<usize, ElfError> {
    let shdr_bytes = headers.read_section_header_bytes(src)?;
    if shdr_bytes.is_empty() {
        return Ok(0);
    }

    // .dynsym first: the symbol-carrying relocation types need it.
    let mut dynsym_bytes = None;
    for shdr in headers.sections(&shdr_bytes).iter() {
        if shdr.sh_type == elf::abi::SHT_DYNSYM {
            dynsym_bytes = Some(src.read_at(shdr.sh_offset as usize, shdr.sh_size as usize)?);
            break;
        }
    }

    let mut applied = 0usize;
    for shdr in headers.sections(&shdr_bytes).iter() {
        if shdr.sh_type != elf::abi::SHT_RELA {
            continue;
        }
        let rela_bytes = src.read_at(shdr.sh_offset as usize, shdr.sh_size as usize)?;

        for rela in headers.relas(&rela_bytes).iter() {
            let vaddr = base + rela.r_offset as usize;
            let page_va = vaddr & !(PAGE_SIZE - 1);
            let Some(&pa) = mapped_pages.get(&page_va) else {
                continue;
            };

            // r_addend is signed; the cast sign-extends, so negative addends
            // wrap correctly under the wrapping adds below.
            let addend = rela.r_addend as usize;
            let value = match rela.r_type {
                R_AARCH64_RELATIVE => base.wrapping_add(addend),
                R_AARCH64_ABS64 | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT => {
                    if rela.r_sym == 0 {
                        if rela.r_type != R_AARCH64_ABS64 {
                            continue;
                        }
                        base.wrapping_add(addend)
                    } else {
                        let sym = dynsym_bytes
                            .as_ref()
                            .and_then(|b| headers.symbols(b.as_ref()).get(rela.r_sym as usize).ok());
                        let Some(sym) = sym else { continue };
                        base.wrapping_add(sym.st_value as usize).wrapping_add(addend)
                    }
                }
                _ => continue,
            };

            crate::frame_write(pa, vaddr & (PAGE_SIZE - 1), &value.to_ne_bytes());
            applied += 1;
        }
    }

    Ok(applied)
}

/// Register the holes between PT_LOAD segments as zero-fill lazy regions.
fn push_gap_regions(
    phdrs: &[ProgramHeader],
    base: usize,
    deferred_segments: &mut Vec<DeferredLazySegment>,
) {
    let mut load_segments: Vec<(usize, usize)> = phdrs
        .iter()
        .filter(|p| p.p_type == PT_LOAD)
        .map(|p| {
            let va = base + p.p_vaddr as usize;
            let end = va + p.p_memsz as usize;
            (va & !(PAGE_SIZE - 1), (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1))
        })
        .collect();
    load_segments.sort_by_key(|&(start, _)| start);

    for w in load_segments.windows(2) {
        let prev_end = w[0].1;
        let next_start = w[1].0;
        if prev_end < next_start {
            let gap_size = next_start - prev_end;
            deferred_segments.push(DeferredLazySegment {
                start_va: prev_end,
                size: gap_size,
                page_flags: user_flags::RW_NO_EXEC,
                file_source: None,
            });
            if DEBUG_ELF_LOADING {
                log::debug!(
                    "[ELF] Gap region (deferred): 0x{:08x}-0x{:08x} ({} pages)",
                    prev_end,
                    next_start,
                    gap_size / PAGE_SIZE
                );
            }
        }
    }
}

/// Effective fill length for a deferred (demand-paged) PT_LOAD segment.
///
/// When a segment's file-backed data ends part-way through a page and a
/// *following* PT_LOAD segment begins in that same page, the demand-pager
/// maps the page on whichever segment faults first, fills it only up to that
/// segment's `filesz`, and zero-fills the remainder of the page. For the
/// earlier segment that zero-fill clobbers the next segment's file-backed
/// bytes (whose own lazy region then sees the page already mapped and never
/// fills it) — silently corrupting e.g. `.rodata` that abuts `.text`.
///
/// ELF `p_offset`s are contiguous, so the bytes the next segment needs sit
/// right after this segment's in the file. We therefore extend this segment's
/// fill to the end of the shared page (bounded by the next segment's own file
/// extent, so we never read past real file data into what must stay zeroed
/// `.bss`). `pt_loads` is `(start_va, file_end_va)` for every PT_LOAD segment.
pub(super) fn boundary_extended_filesz(
    vaddr: usize,
    filesz: usize,
    pt_loads: &[(usize, usize)],
) -> usize {
    let seg_last_page_end = (vaddr + filesz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut eff = filesz;
    for &(next_va, next_file_end) in pt_loads {
        if next_va > vaddr && next_va < seg_last_page_end {
            let extend_to = core::cmp::min(seg_last_page_end, next_file_end);
            eff = eff.max(extend_to - vaddr);
        }
    }
    eff
}

#[cfg(test)]
mod boundary_tests {
    use super::boundary_extended_filesz;
    use akuma_mmap::PAGE_SIZE;

    // Regression: the demand-paged ELF loader zeroed a following segment's
    // file-backed bytes that shared a page with the prior segment, corrupting
    // .rodata (manifested as empty userspace `format!` output on the size/
    // extreme kernel — meow's "Failed to create request buffer"). Mirrors
    // meow's real layout: R-X seg ends at 0x4375D8, R-- seg starts at 0x437558
    // (same page 0x437000), R-W seg is page-aligned at 0x444000.
    #[test]
    fn extends_fill_across_shared_boundary_page() {
        let seg1 = (0x400000usize, 0x4375D8usize); // R-X, ends mid-page
        let seg2 = (0x437558usize, 0x443274usize); // R--, starts in seg1's last page
        let seg3 = (0x444000usize, 0x4441AAusize); // R-W, page-aligned
        let pt = [seg1, seg2, seg3];

        // seg1 must be extended to fill the whole shared page (0x438000) so the
        // pager populates seg2's rodata bytes [0x4375D8, 0x438000) from file.
        let f1 = boundary_extended_filesz(seg1.0, seg1.1 - seg1.0, &pt);
        assert_eq!(seg1.0 + f1, 0x438000, "seg1 fill should reach shared page end");
    }

    #[test]
    fn does_not_extend_when_next_segment_is_page_aligned() {
        // seg3 is page-aligned and has trailing .bss; its fill must NOT be
        // extended (extending would read file garbage into .bss).
        let seg2 = (0x437558usize, 0x443274usize);
        let seg3 = (0x444000usize, 0x4441AAusize);
        let pt = [seg2, seg3];
        let f3 = boundary_extended_filesz(seg3.0, seg3.1 - seg3.0, &pt);
        assert_eq!(f3, seg3.1 - seg3.0, "page-aligned successor → no extension");
    }

    #[test]
    fn caps_extension_at_next_segment_file_end() {
        // If the next segment's file data ends before the page boundary, the
        // extension must stop there (the tail is real .bss, must stay zero).
        let a = (0x1000usize, 0x1f00usize);  // ends mid-page (page 0x1000-0x2000)
        let b = (0x1f80usize, 0x1fc0usize);  // starts in same page, short filesz
        let pt = [a, b];
        let fa = boundary_extended_filesz(a.0, a.1 - a.0, &pt);
        assert_eq!(a.0 + fa, 0x1fc0, "extension capped at next seg file end");
        assert!(a.0 + fa < a.0 + PAGE_SIZE, "must not reach full page when capped");
    }

    #[test]
    fn no_following_segment_is_unchanged() {
        let only = (0x400000usize, 0x437000usize);
        let pt = [only];
        let f = boundary_extended_filesz(only.0, only.1 - only.0, &pt);
        assert_eq!(f, only.1 - only.0);
    }
}
