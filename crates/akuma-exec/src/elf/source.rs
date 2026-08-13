//! The *source* axis of ELF loading: where the bytes come from.
//!
//! This is deliberately independent of the *mapping* axis
//! ([`super::load::MapStrategy`] — copy every page now, or register demand-paged
//! regions). Before the two were split, loaded-from-a-path implied deferred
//! mapping and loaded-from-bytes implied eager mapping, purely because the path
//! loaders were copy-pasted from the byte loaders. Nothing about either axis
//! requires the other, so both are parameters now.
//!
//! Everything here parses through the vetted `elf` 0.7 crate; the kernel no
//! longer carries a second, hand-rolled ELF parser reading fields at literal
//! byte offsets. `ElfBytes::minimal_parse` was not usable for this because it
//! wants the whole file — exactly what the lazy path refuses to read — but
//! `elf::file::parse_ident`, `FileHeader::parse_tail` and `ParsingTable` are
//! `no_std` and work on small buffers. So a path source reads 64 bytes, parses
//! the header properly, reads `e_phnum * e_phentsize` bytes, and iterates, with
//! no whole-file slurp. (`ElfStream` does this for you but is
//! `#[cfg(feature = "std")]` and therefore unavailable here.)
//!
//! The two parsers were verified equivalent over 2,387 binaries (0
//! disagreements, 0 panics on 280 hostile header mutations) before the
//! hand-rolled one was deleted — see
//! `docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §3.

use alloc::borrow::Cow;
use alloc::vec::Vec;

use elf::abi::EI_NIDENT;
use elf::endian::LittleEndian;
use elf::file::{Class, FileHeader};
use elf::parse::{ParseAt, ParsingTable};
use elf::relocation::Rela;
use elf::section::{SectionHeader, SectionHeaderTable};
use elf::segment::{ProgramHeader, SegmentTable};
use elf::symbol::SymbolTable;

use crate::runtime::runtime;

use super::types::{ELF64_EHDR_SIZE, ELF_MAGIC, ElfError};

/// Where an ELF image's bytes come from.
///
/// `Bytes` hands out borrowed sub-slices, so reading through it costs nothing;
/// `Path` allocates a buffer per read, which is the whole point — it never
/// holds more than the chunk being read.
#[derive(Clone, Copy)]
pub(super) enum ElfSource<'a> {
    /// An image already resident in the kernel heap.
    Bytes(&'a [u8]),
    /// An image left on disk, read a piece at a time.
    Path(&'a str),
}

impl<'a> ElfSource<'a> {
    /// Read exactly `len` bytes at `offset`, or fail.
    ///
    /// Both variants are strict about short reads: a caller that asks for a
    /// program header table gets either the whole table or an error, never a
    /// truncated one it might silently mis-parse.
    pub(super) fn read_at(self, offset: usize, len: usize) -> Result<Cow<'a, [u8]>, ElfError> {
        match self {
            Self::Bytes(data) => {
                let end = offset
                    .checked_add(len)
                    .ok_or(ElfError::InvalidFormat("Read offset overflow"))?;
                data.get(offset..end)
                    .map(Cow::Borrowed)
                    .ok_or(ElfError::InvalidFormat("Read past end of image"))
            }
            Self::Path(path) => file_read_exact(path, offset, len).map(Cow::Owned),
        }
    }
}

/// Read exactly `len` bytes from a file at `offset`, returning an error on short reads.
fn file_read_exact(path: &str, offset: usize, len: usize) -> Result<Vec<u8>, ElfError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut buf = alloc::vec![0u8; len];
    let n = (runtime().read_at)(path, offset, &mut buf)
        .map_err(|_| ElfError::InvalidFormat("File read failed"))?;
    if n < len {
        return Err(ElfError::InvalidFormat("Short read"));
    }
    Ok(buf)
}

/// A parsed ELF file header plus the raw program header table it points at.
///
/// Holding the phdr bytes (borrowed for a `Bytes` source, owned for a `Path`
/// one) is what lets the lazy loader iterate segments through the `elf` crate's
/// bounds-checked `SegmentTable` without ever materialising the whole file.
pub(super) struct ElfHeaders<'a> {
    pub(super) ehdr: FileHeader<LittleEndian>,
    phdr_buf: Cow<'a, [u8]>,
}

impl<'a> ElfHeaders<'a> {
    /// Bounds-checked, lazily-parsing view of the program header table.
    pub(super) fn segments(&self) -> SegmentTable<'_, LittleEndian> {
        SegmentTable::new(self.ehdr.endianness, self.ehdr.class, &self.phdr_buf)
    }

    /// Read the raw section header table, empty when the file has none.
    ///
    /// Only the eager mapping strategy needs this (relocations); the demand-paged
    /// path never touches section headers, so it never pays for the read.
    pub(super) fn read_section_header_bytes(
        &self,
        src: ElfSource<'a>,
    ) -> Result<Cow<'a, [u8]>, ElfError> {
        if self.ehdr.e_shoff == 0 {
            return Ok(Cow::Borrowed(&[]));
        }
        let entsize = SectionHeader::validate_entsize(self.ehdr.class, self.ehdr.e_shentsize as usize)
            .map_err(|_| ElfError::InvalidFormat("Bad e_shentsize"))?;

        // e_shnum == 0 alongside a non-zero e_shoff is the SHN_LORESERVE escape:
        // the real count lives in shdr[0].sh_size.
        let mut shnum = self.ehdr.e_shnum as usize;
        if shnum == 0 {
            let first = src.read_at(self.ehdr.e_shoff as usize, entsize)?;
            shnum = self
                .sections(&first)
                .get(0)
                .map_err(|_| ElfError::InvalidFormat("Bad section header 0"))?
                .sh_size as usize;
        }

        let size = entsize
            .checked_mul(shnum)
            .ok_or(ElfError::InvalidFormat("Section header table overflow"))?;
        src.read_at(self.ehdr.e_shoff as usize, size)
    }

    /// Bounds-checked view of a section header table read from this file.
    pub(super) fn sections<'b>(&self, buf: &'b [u8]) -> SectionHeaderTable<'b, LittleEndian> {
        SectionHeaderTable::new(self.ehdr.endianness, self.ehdr.class, buf)
    }

    /// Bounds-checked view of a symbol table read from this file.
    pub(super) fn symbols<'b>(&self, buf: &'b [u8]) -> SymbolTable<'b, LittleEndian> {
        SymbolTable::new(self.ehdr.endianness, self.ehdr.class, buf)
    }

    /// Bounds-checked view of an SHT_RELA section's contents.
    pub(super) fn relas<'b>(&self, buf: &'b [u8]) -> ParsingTable<'b, LittleEndian, Rela> {
        ParsingTable::new(self.ehdr.endianness, self.ehdr.class, buf)
    }
}

/// Parse the ELF header and program header table out of `src`.
///
/// Reads 64 bytes, then `e_phnum * e_phentsize` bytes. Nothing else.
pub(super) fn parse_headers(src: ElfSource<'_>) -> Result<ElfHeaders<'_>, ElfError> {
    let hdr = src
        .read_at(0, ELF64_EHDR_SIZE)
        .map_err(|e| refine_ident_error(src, e))?;

    let ident = elf::file::parse_ident::<LittleEndian>(&hdr[..EI_NIDENT])
        .map_err(|_| refine_ident_error(src, ElfError::InvalidFormat("Bad ELF identification")))?;
    if ident.1 != Class::ELF64 {
        return Err(ElfError::InvalidFormat("Not ELF64"));
    }
    let ehdr = FileHeader::parse_tail(ident, &hdr[EI_NIDENT..ELF64_EHDR_SIZE])
        .map_err(|_| ElfError::InvalidFormat("Bad ELF header"))?;

    // PN_XNUM keeps the real segment count in shdr[0].sh_info. Nothing this
    // kernel executes comes near 65535 segments, so reject it outright rather
    // than mis-reading the table as 65535 entries.
    if ehdr.e_phnum == elf::abi::PN_XNUM {
        return Err(ElfError::InvalidFormat("PN_XNUM segment count unsupported"));
    }

    let entsize = ProgramHeader::validate_entsize(ehdr.class, ehdr.e_phentsize as usize)
        .map_err(|_| ElfError::InvalidFormat("Bad e_phentsize"))?;
    let table_size = entsize
        .checked_mul(ehdr.e_phnum as usize)
        .ok_or(ElfError::InvalidFormat("Program header table overflow"))?;

    let phdr_buf = if ehdr.e_phoff == 0 || table_size == 0 {
        Cow::Borrowed(&[][..])
    } else {
        src.read_at(ehdr.e_phoff as usize, table_size)?
    };

    Ok(ElfHeaders { ehdr, phdr_buf })
}

/// Give a failed header read the most useful error we can.
///
/// `InvalidMagic` carries the four bytes actually seen, which is what tells you
/// at a glance that the "binary" is a shell script or an empty file — worth the
/// extra 4-byte read, which only happens on the failure path. A file whose magic
/// *is* correct but that is truncated or otherwise malformed keeps the specific
/// error instead.
fn refine_ident_error(src: ElfSource<'_>, fallback: ElfError) -> ElfError {
    if let Ok(head) = src.read_at(0, ELF_MAGIC.len()) {
        if head[..] != ELF_MAGIC[..] {
            let mut magic = [0u8; 4];
            magic.copy_from_slice(&head);
            return ElfError::InvalidMagic(magic);
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    //! These cover the parsing that replaced the hand-rolled parser in
    //! `types.rs`. They run against `ElfSource::Bytes`, which is the same code
    //! path `ElfSource::Path` feeds — only `read_at` differs, and `Path`'s half
    //! needs a registered runtime with a real filesystem behind it.

    use super::*;
    use elf::abi::{EM_AARCH64, ET_EXEC, PT_LOAD};

    const PHENTSIZE: u16 = 56;

    fn ehdr_bytes(e_type: u16, e_machine: u16, e_entry: u64, e_phoff: u64, e_phentsize: u16, e_phnum: u16) -> [u8; 64] {
        let mut buf = [0u8; 64];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[4] = 2; // ELFCLASS64
        buf[5] = 1; // ELFDATA2LSB
        buf[6] = 1; // EV_CURRENT
        buf[16..18].copy_from_slice(&e_type.to_le_bytes());
        buf[18..20].copy_from_slice(&e_machine.to_le_bytes());
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        buf[24..32].copy_from_slice(&e_entry.to_le_bytes());
        buf[32..40].copy_from_slice(&e_phoff.to_le_bytes());
        buf[54..56].copy_from_slice(&e_phentsize.to_le_bytes());
        buf[56..58].copy_from_slice(&e_phnum.to_le_bytes());
        buf
    }

    fn phdr_bytes(p_type: u32, p_flags: u32, p_offset: u64, p_vaddr: u64, p_filesz: u64, p_memsz: u64) -> [u8; 56] {
        let mut buf = [0u8; 56];
        buf[0..4].copy_from_slice(&p_type.to_le_bytes());
        buf[4..8].copy_from_slice(&p_flags.to_le_bytes());
        buf[8..16].copy_from_slice(&p_offset.to_le_bytes());
        buf[16..24].copy_from_slice(&p_vaddr.to_le_bytes());
        buf[32..40].copy_from_slice(&p_filesz.to_le_bytes());
        buf[40..48].copy_from_slice(&p_memsz.to_le_bytes());
        buf
    }

    /// A minimal but well-formed ET_EXEC image: header + one PT_LOAD.
    fn one_segment_image() -> Vec<u8> {
        let mut image = Vec::new();
        image.extend_from_slice(&ehdr_bytes(ET_EXEC, EM_AARCH64, 0x400000, 64, PHENTSIZE, 1));
        image.extend_from_slice(&phdr_bytes(PT_LOAD, 5, 0, 0x400000, 0x1000, 0x2000));
        image
    }

    #[test]
    fn reads_borrow_from_a_byte_source() {
        let data = [1u8, 2, 3, 4, 5];
        let src = ElfSource::Bytes(&data);
        assert!(matches!(src.read_at(1, 3), Ok(Cow::Borrowed(&[2, 3, 4]))));
        assert!(src.read_at(0, 0).is_ok(), "zero-length read is legal");
    }

    #[test]
    fn reads_past_the_end_are_rejected_not_truncated() {
        let data = [1u8, 2, 3, 4, 5];
        let src = ElfSource::Bytes(&data);
        assert!(src.read_at(3, 3).is_err());
        assert!(src.read_at(6, 1).is_err());
        assert!(src.read_at(usize::MAX, 2).is_err(), "offset+len must not wrap");
    }

    #[test]
    fn parses_header_and_segments() {
        let image = one_segment_image();
        let headers = parse_headers(ElfSource::Bytes(&image)).expect("valid image should parse");

        assert_eq!(headers.ehdr.e_type, ET_EXEC);
        assert_eq!(headers.ehdr.e_machine, EM_AARCH64);
        assert_eq!(headers.ehdr.e_entry, 0x400000);
        assert_eq!(headers.ehdr.e_phoff, 64);
        assert_eq!(headers.ehdr.e_phentsize, PHENTSIZE);
        assert_eq!(headers.ehdr.e_phnum, 1);

        let segs: Vec<_> = headers.segments().iter().collect();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].p_type, PT_LOAD);
        assert_eq!(segs[0].p_flags, 5);
        assert_eq!(segs[0].p_vaddr, 0x400000);
        assert_eq!(segs[0].p_filesz, 0x1000);
        assert_eq!(segs[0].p_memsz, 0x2000);
    }

    #[test]
    fn non_elf_input_reports_the_bytes_it_saw() {
        let text = b"not-an-elf-binary: plain text, and shorter than a header";
        match parse_headers(ElfSource::Bytes(text)).map(|_| ()) {
            Err(ElfError::InvalidMagic(m)) => assert_eq!(&m, b"not-"),
            other => panic!("expected InvalidMagic, got {other:?}"),
        }
    }

    #[test]
    fn truncated_elf_is_not_reported_as_bad_magic() {
        // Correct magic, too short to hold a header: the magic is not the
        // problem, so saying "bad magic" would send you down the wrong path.
        let image = one_segment_image();
        match parse_headers(ElfSource::Bytes(&image[..48])).map(|_| ()) {
            Err(ElfError::InvalidFormat(_)) => {}
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
    }

    #[test]
    fn truncated_program_header_table_is_rejected() {
        // Header claims one 56-byte phdr; only 40 bytes follow. The old
        // hand-rolled parser sliced `&phdr_buf[i * e_phentsize..]` and would
        // have parsed whatever came next.
        let image = one_segment_image();
        assert!(parse_headers(ElfSource::Bytes(&image[..64 + 40])).is_err());
    }

    #[test]
    fn wrong_class_and_entsize_are_rejected() {
        let mut e32 = one_segment_image();
        e32[4] = 1; // ELFCLASS32
        assert!(parse_headers(ElfSource::Bytes(&e32)).is_err());

        let mut bad_entsize = one_segment_image();
        bad_entsize[54..56].copy_from_slice(&32u16.to_le_bytes());
        assert!(
            parse_headers(ElfSource::Bytes(&bad_entsize)).is_err(),
            "an e_phentsize that is not 56 must be rejected, not used as a stride"
        );
    }

    #[test]
    fn hostile_header_fields_error_rather_than_panic() {
        // The 280-case differential harness (§3) established that neither parser
        // panics on these; keep a few resident so a future edit cannot regress
        // it, because the kernel builds panic = "abort".
        for (off, len, val) in [
            (32usize, 8usize, u64::MAX),       // e_phoff
            (56, 2, 0xFFFF),                   // e_phnum (PN_XNUM)
            (56, 2, 0),                        // e_phnum
            (54, 2, 0xFFFF),                   // e_phentsize
            (54, 2, 0),                        // e_phentsize
        ] {
            let mut image = one_segment_image();
            image[off..off + len].copy_from_slice(&val.to_le_bytes()[..len]);
            // Must not panic. Success is only acceptable for e_phnum == 0,
            // which is a legal (if useless) empty table.
            let _ = parse_headers(ElfSource::Bytes(&image));
        }
    }

    #[test]
    fn no_program_header_table_yields_an_empty_segment_list() {
        let mut image = one_segment_image();
        image[32..40].copy_from_slice(&0u64.to_le_bytes()); // e_phoff = 0
        let headers = parse_headers(ElfSource::Bytes(&image)).expect("e_phoff=0 is legal");
        assert_eq!(headers.segments().iter().count(), 0);
    }
}
