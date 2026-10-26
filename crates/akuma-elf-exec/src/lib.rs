//! The pure "what runs, and from where" decisions for an ELF exec.
//!
//! Three loaders in this tree place programs (the amd64 kernel through
//! `akuma-elf`, the minimal `taoxichuan` composition through
//! `akuma-ring3-amd64`, and historically a second, standalone amd64 loader).
//! The *mapping* of each is deliberately its own — different page-table
//! plumbing, different file sources, different stack builders — but the
//! decisions in front of the mapping are one body of fact, and before this
//! crate they were written out twice with different shapes:
//!
//! * a ~14-line `PT_INTERP` scan in `akuma-elf/src/load.rs` (which carries
//!   the one rule this crate exists for: a `PT_INTERP` of `p_filesz <= 1`
//!   is what a **static-PIE** emits, not an interpreter name — read as a
//!   path it would turn into a failed file read for a binary that needs
//!   none);
//! * the entry decision in `amd64/src/loader.rs`'s `finish()` ("ring 3 is
//!   entered in the linker, which brings the program up itself" — the cross
//!   target is the interpreter's entry when one was mapped, while `AT_ENTRY`
//!   stays the *program's*);
//! * and nothing at all in `taoxichuan`, whose `AT_BASE` was hard 0.
//!
//! So the rule lived in one consumer and was missing from the other, which
//! is exactly backwards for a fact about ELF files rather than about kernels.
//! This crate holds the fact once: bounds-checked header/program-header
//! reads over plain `&[u8]`, the `p_filesz <= 1` static-PIE rule, rebased
//! `PT_LOAD` placement facts for the consumer that maps, and the entry to
//! cross. Every effect — page tables, file reads, stack layout, auxv *table*
//! shapes — stays with the caller. `#![forbid(unsafe_code)]` and zero
//! dependencies: the decisions are slices and integers, testable on the host.
#![no_std]
#![forbid(unsafe_code)]

/// Where a dynamically linked image's interpreter is placed.
///
/// The same base `akuma-elf` places
/// the interpreter at: above a static-PIE program at `PIE_BASE = 0x1000_0000`
/// (an image would have to be 512 MiB to reach it) and far below every
/// kernel's mmap window.
pub const INTERP_BASE: u64 = 0x3000_0000;

/// What refused an image. One variant per distinct refusal, so a caller's
/// error path can name what happened without this crate formatting strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecErr {
    /// Missing or wrong `\x7fELF` magic.
    Magic,
    /// Not ELF64 little-endian (`EI_CLASS`/`EI_DATA`).
    NotElf64Le,
    /// Neither `ET_EXEC` nor `ET_DYN`.
    NotExec,
    /// The program header table is missing, mis-sized, out of bounds, or
    /// `e_phnum` is the `PN_XNUM` escape (a real header table per phdr, not
    /// a count in a field — refused, as `akuma-elf`'s parser refuses it).
    PhdrTable,
    /// `e_entry == 0`: nowhere to enter.
    NoEntry,
    /// More `PT_LOAD` segments than the caller's output slice holds.
    TooManySegments,
}

/// The `PT_INTERP` segment type.
pub const PT_INTERP: u32 = 3;
/// `p_flags`: executable / writable (the prot mapping is the caller's).
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;

/// One `PT_LOAD` at its runtime address: the facts a consumer needs to map
/// a segment — nothing about *how* to map it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSeg {
    /// `p_vaddr` rebased by the caller's base (0 for an `ET_EXEC` image).
    pub vaddr: u64,
    /// `p_offset`: where in the image this segment's file-backed bytes start.
    /// A mapper indexing the whole image Vec must add this; indexing with the
    /// vaddr-relative offset alone reads the wrong slice whenever
    /// `p_offset != p_vaddr` — every Alpine dynamic binary's text segment.
    pub offset: u64,
    /// File-backed bytes.
    pub filesz: u64,
    /// In-memory span (`memsz >= filesz`; the tail is zero-fill).
    pub memsz: u64,
    /// Raw `p_flags` (`PF_X|PF_W|PF_R`) — prot mapping is the caller's policy.
    pub flags: u32,
}

/// ELF class/data/type positions this crate actually reads. ELF64 phdr size,
/// fixed by the ABI; a table with any other stride is refused, not guessed.
const PHENT: u64 = 56;
const PT_LOAD: u32 = 1;
/// The `e_phnum` escape value meaning "the real count is in section header
/// 0" — no image this serves has it, and honoring it would mean a second
/// lookup this crate has no use for.
const PN_XNUM: u16 = 0xffff;

/// Bounds-checked little-endian reads: `None` on any read past `image`.
fn rd16(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn rd32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn rd64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

/// Validate the ELF identity and hand back the program-header table facts.
/// `(e_entry, phoff, phentsize, phnum)`.
fn header(image: &[u8]) -> Result<(u64, u64, u64, u64), ExecErr> {
    if image.len() < 64 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ExecErr::Magic);
    }
    // EI_CLASS = 2 (ELF64), EI_DATA = 1 (two's complement, little-endian) —
    // the same two refusals `akuma-ring3-amd64` and `akuma-elf` make.
    if image[4] != 2 || image[5] != 1 {
        return Err(ExecErr::NotElf64Le);
    }
    let e_type = rd16(image, 16).ok_or(ExecErr::Magic)?;
    if e_type != 2 && e_type != 3 {
        return Err(ExecErr::NotExec);
    }
    let e_entry = rd64(image, 24).ok_or(ExecErr::Magic)?;
    let phoff = rd64(image, 32).ok_or(ExecErr::Magic)?;
    let phentsize = u64::from(rd16(image, 54).ok_or(ExecErr::Magic)?);
    let phnum = u64::from(rd16(image, 56).ok_or(ExecErr::Magic)?);
    if phentsize != PHENT || phnum == 0 || phnum == u64::from(PN_XNUM) {
        return Err(ExecErr::PhdrTable);
    }
    // The whole table must sit inside the image: every phdr read below is
    // bounds-checked anyway, but refusing here names the problem once.
    if phoff == 0 || phoff.saturating_add(phnum * PHENT) > image.len() as u64 {
        return Err(ExecErr::PhdrTable);
    }
    Ok((e_entry, phoff, phentsize, phnum))
}

/// Iterate the phdr table as
/// `(p_type, p_offset, p_filesz, p_vaddr, p_flags, p_memsz)` tuples, each
/// read bounds-checked against `image`.
#[allow(clippy::type_complexity)]
fn phdrs(
    image: &[u8],
) -> Result<impl Iterator<Item = (u32, u64, u64, u64, u32, u64)>, ExecErr> {
    let (_, phoff, phentsize, phnum) = header(image)?;
    let n = phnum as usize;
    let step = phentsize as usize;
    Ok((0..n).filter_map(move |i| {
        let at = phoff as usize + i * step;
        Some((
            rd32(image, at)?,
            rd64(image, at + 8)?,
            rd64(image, at + 32)?,
            rd64(image, at + 16)?,
            rd32(image, at + 4)?,
            rd64(image, at + 40)?,
        ))
    }))
}

/// The interpreter-path decision for the bytes of ONE `PT_INTERP` segment.
///
/// `Some` NUL-stripped path bytes when the segment names an interpreter —
/// `p_filesz > 1` (a one-NUL-byte `PT_INTERP` is what a **static-PIE**
/// emits, not a path; read as one it becomes a failed file read for a
/// binary that needs no interpreter). An absent NUL is tolerated, as the
/// scan this replaces tolerated it. UTF-8 is the caller's check.
///
/// Consumers with a parsed phdr table already in hand (`akuma-elf`'s
/// `elf`-crate parse) call this per segment; consumers reading raw bytes
/// call [`interp_path`], which walks the table itself with this rule.
#[must_use]
pub fn interp_bytes(p_filesz: u64, seg: &[u8]) -> Option<&[u8]> {
    if p_filesz <= 1 {
        return None;
    }
    let bytes = &seg[..seg.len().min(p_filesz as usize)];
    Some(if bytes.last() == Some(&0) { &bytes[..bytes.len() - 1] } else { bytes })
}

/// Read the interpreter path a `PT_INTERP` segment names.
///
/// * `Ok(None)` — statically linked, **including** the one-NUL-byte
///   `PT_INTERP` a static-PIE emits (`p_filesz <= 1`): nothing to load.
/// * `Ok(Some(bytes))` — the path bytes, trailing NUL stripped (absent NUL
///   tolerated, as `akuma-elf`'s scan tolerates it). UTF-8 is the caller's
///   check: this crate stays byte-shaped.
/// * `Err(..)` — not a parseable ELF64 executable image.
pub fn interp_path(image: &[u8]) -> Result<Option<&[u8]>, ExecErr> {
    let mut found = None;
    for (ty, off, filesz, _, _, _) in phdrs(image)? {
        if ty == PT_INTERP {
            // The static-PIE rule decides before any byte is touched: a
            // `p_filesz <= 1` segment is "no interpreter" wherever it sits.
            if filesz <= 1 {
                continue;
            }
            let end = off.saturating_add(filesz);
            if end > image.len() as u64 {
                // A path running past the image is a malformed segment, not
                // "static" — refuse rather than guess.
                return Err(ExecErr::PhdrTable);
            }
            if let Some(bytes) = interp_bytes(filesz, &image[off as usize..end as usize]) {
                found = Some(bytes);
            }
        }
    }
    Ok(found)
}

/// The `PT_LOAD` segments of `image`, rebased by `base`.
///
/// Filled into `out`; returns how many there are. The caller picks `base` — `0` for an
/// `ET_EXEC` image at its link addresses, [`INTERP_BASE`] (or its own) for an
/// `ET_DYN` one. Segments beyond `out.len()` are a [`ExecErr::
/// TooManySegments`], not a silent truncation.
pub fn load_segments(image: &[u8], base: u64, out: &mut [LoadSeg]) -> Result<usize, ExecErr> {
    let mut n = 0;
    for (ty, offset, filesz, vaddr, flags, memsz) in phdrs(image)? {
        if ty != PT_LOAD {
            continue;
        }
        if n >= out.len() {
            return Err(ExecErr::TooManySegments);
        }
        out[n] = LoadSeg { vaddr: vaddr.wrapping_add(base), offset, filesz, memsz, flags };
        n += 1;
    }
    Ok(n)
}

/// The interpreter's entry once mapped at `base` (`ET_DYN`: `e_entry` is a
/// link address, so the runtime entry is `e_entry + base`).
pub fn interp_entry(image: &[u8], base: u64) -> Result<u64, ExecErr> {
    let (e_entry, ..) = header(image)?;
    if e_entry == 0 {
        return Err(ExecErr::NoEntry);
    }
    Ok(e_entry.wrapping_add(base))
}

/// Which entry ring 3 crosses: the interpreter's when one was mapped — the
/// linker brings the program up itself — else the program's. (`AT_ENTRY`
/// stays the *program's* either way; that is a separate fact.)
#[must_use]
pub fn entry_to_cross(has_interp: bool, prog_entry: u64, interp_entry: u64) -> u64 {
    if has_interp { interp_entry } else { prog_entry }
}

/// The `AT_BASE` value: where the interpreter was placed, 0 when none —
/// the same "no interpreter" answer a static image's auxv has always carried.
#[must_use]
pub fn at_base(has_interp: bool, interp_base: u64) -> u64 {
    if has_interp { interp_base } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a minimal ELF64 header + `phdrs` program headers into `buf`.
    /// Returns the total length written.
    fn elf(buf: &mut [u8], e_type: u16, entry: u64, phdrs: &[(u32, u64, u64, u64, u32, u64)]) -> usize {
        buf.fill(0);
        buf[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        buf[4] = 2; // ELF64
        buf[5] = 1; // little-endian
        buf[16..18].copy_from_slice(&e_type.to_le_bytes());
        buf[24..32].copy_from_slice(&entry.to_le_bytes());
        let phoff = 64u64;
        buf[32..40].copy_from_slice(&phoff.to_le_bytes());
        buf[54..56].copy_from_slice(&56u16.to_le_bytes());
        buf[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
        let mut at = phoff as usize;
        for (ty, off, filesz, vaddr, flags, memsz) in phdrs {
            buf[at..at + 4].copy_from_slice(&ty.to_le_bytes());
            buf[at + 4..at + 8].copy_from_slice(&flags.to_le_bytes());
            buf[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
            buf[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            buf[at + 32..at + 40].copy_from_slice(&filesz.to_le_bytes());
            buf[at + 40..at + 48].copy_from_slice(&memsz.to_le_bytes());
            at += 56;
        }
        at
    }

    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;

    #[test]
    fn static_exec_has_no_interp() {
        let mut buf = [0u8; 256];
        let n = elf(&mut buf, 2, 0x40_1000, &[(PT_LOAD, 0, 0x1000, 0x40_0000, 5, 0x1000)]);
        assert_eq!(interp_path(&buf[..n]), Ok(None));
    }

    #[test]
    fn one_nul_interp_is_a_static_pie_not_a_path() {
        // What a static-PIE emits: PT_INTERP naming nothing (filesz == 1).
        let mut buf = [0u8; 256];
        let n = elf(&mut buf, 3, 0x1000_1000, &[(PT_INTERP, 0x200, 1, 0, 4, 1)]);
        assert_eq!(interp_path(&buf[..n]), Ok(None));
        // The same rule at the per-segment seam akuma-elf rides.
        assert_eq!(interp_bytes(1, &[0]), None);
        assert_eq!(interp_bytes(0, &[]), None);
        assert_eq!(interp_bytes(4, b"/ld\0"), Some(&b"/ld"[..]));
    }

    #[test]
    fn interp_path_is_the_nul_stripped_bytes() {
        let mut buf = [0u8; 1024];
        let path_at = 0x300u64;
        let p = b"/lib/ld-musl-x86_64.so.1\0";
        let n0 = elf(
            &mut buf,
            3,
            0x1560,
            &[(PT_LOAD, 0, 0x2000, 0, 5, 0x2000), (PT_INTERP, path_at, p.len() as u64, 0x2f0, 4, p.len() as u64)],
        );
        buf[path_at as usize..path_at as usize + p.len()].copy_from_slice(p);
        assert!(n0 > 0);
        assert_eq!(interp_path(&buf), Ok(Some(&b"/lib/ld-musl-x86_64.so.1"[..])));
        // No trailing NUL: the bytes come back as they are (akuma-elf parity).
        buf[path_at as usize + p.len() - 1] = b'2';
        assert_eq!(interp_path(&buf), Ok(Some(&b"/lib/ld-musl-x86_64.so.12"[..])));
    }

    #[test]
    fn interp_path_past_the_image_is_a_refusal_not_static() {
        let mut buf = [0u8; 256];
        let n = elf(&mut buf, 3, 0x1000, &[(PT_INTERP, 0x200, 4096, 0, 4, 4096)]);
        assert_eq!(interp_path(&buf[..n]), Err(ExecErr::PhdrTable));
    }

    #[test]
    fn segments_rebase_by_the_callers_base() {
        let mut buf = [0u8; 512];
        let n = elf(
            &mut buf,
            3,
            0x1560,
            &[
                (PT_LOAD, 0, 0x1000, 0, 5, 0x1000),
                (PT_INTERP, 0x2f0, 10, 0x2f0, 4, 10),
                (PT_LOAD, 0x1000, 0x240, 0x2000, 6, 0x2c8),
            ],
        );
        let mut segs = [LoadSeg {
            vaddr: 0,
            filesz: 0,
            memsz: 0,
            flags: 0,
        }; 4];
        let k = load_segments(&buf[..n], 0x3000_0000, &mut segs).unwrap();
        assert_eq!(k, 2);
        assert_eq!(segs[0].vaddr, 0x3000_0000);
        assert_eq!(segs[0].memsz, 0x1000);
        assert_eq!(segs[1].vaddr, 0x3000_2000);
        assert_eq!((segs[1].filesz, segs[1].memsz, segs[1].flags), (0x240, 0x2c8, 6));
    }

    #[test]
    fn too_many_segments_is_a_refusal() {
        let mut buf = [0u8; 512];
        let n = elf(
            &mut buf,
            2,
            0x1000,
            &[(PT_LOAD, 0, 0x10, 0, 5, 0x10), (PT_LOAD, 0, 0x10, 0x2000, 6, 0x10)],
        );
        let mut segs = [LoadSeg { vaddr: 0, filesz: 0, memsz: 0, flags: 0 }; 1];
        assert_eq!(load_segments(&buf[..n], 0, &mut segs), Err(ExecErr::TooManySegments));
    }

    #[test]
    fn entry_and_base_decisions() {
        let mut buf = [0u8; 128];
        let n = elf(&mut buf, 3, 0x1560, &[(PT_LOAD, 0, 0x10, 0, 5, 0x10)]);
        assert_eq!(interp_entry(&buf[..n], INTERP_BASE), Ok(0x3000_1560));
        assert_eq!(entry_to_cross(true, 0x1000_1560, 0x3000_1560), 0x3000_1560);
        assert_eq!(entry_to_cross(false, 0x1000_1560, 0x3000_1560), 0x1000_1560);
        assert_eq!(at_base(true, INTERP_BASE), INTERP_BASE);
        assert_eq!(at_base(false, INTERP_BASE), 0);
    }

    #[test]
    fn refusals_name_the_defect() {
        assert_eq!(interp_path(b""), Err(ExecErr::Magic));
        let mut buf = [0u8; 64];
        buf[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        buf[4] = 2; // ELF64
        buf[5] = 1; // little-endian
        buf[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        assert_eq!(interp_path(&buf), Err(ExecErr::PhdrTable)); // phnum 0
        buf[4] = 9;
        assert_eq!(interp_path(&buf), Err(ExecErr::NotElf64Le));
        buf[4] = 2;
        buf[16..18].copy_from_slice(&4u16.to_le_bytes()); // ET_REL
        assert_eq!(interp_path(&buf), Err(ExecErr::NotExec));
        // Zero entry in a dynamic image: interp_entry refuses; the scan of a
        // static image does not care about entry at all.
        let mut buf2 = [0u8; 128];
        let n = elf(&mut buf2, 3, 0, &[(PT_LOAD, 0, 0x10, 0, 5, 0x10)]);
        assert_eq!(interp_entry(&buf2[..n], INTERP_BASE), Err(ExecErr::NoEntry));
    }
}
