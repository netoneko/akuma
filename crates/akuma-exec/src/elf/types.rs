//! Pure types and constants for the ELF loader.
//!
//! No architecture-specific or runtime dependencies - fully host-testable.

#![allow(dead_code)]

use alloc::string::String;

pub const DEBUG_ELF_LOADING: bool = true;

/// File-backed source for a deferred lazy segment.
pub struct FileSegmentSource {
    pub path: String,
    pub inode: u32,
    pub file_offset: usize,
    pub filesz: usize,
    pub segment_va: usize,
}

/// Segment whose pages will be allocated on first access (demand paging).
pub struct DeferredLazySegment {
    pub start_va: usize,
    pub size: usize,
    pub page_flags: u64,
    pub file_source: Option<FileSegmentSource>,
}

/// Information about a loaded interpreter (dynamic linker)
pub struct InterpInfo {
    pub entry_point: usize,
    pub base_addr: usize,
}

/// Auxiliary Vector entry types
pub mod auxv {
    pub const AT_NULL: u64 = 0;
    pub const AT_IGNORE: u64 = 1;
    pub const AT_EXECFD: u64 = 2;
    pub const AT_PHDR: u64 = 3;
    pub const AT_PHENT: u64 = 4;
    pub const AT_PHNUM: u64 = 5;
    pub const AT_PAGESZ: u64 = 6;
    pub const AT_BASE: u64 = 7;
    pub const AT_FLAGS: u64 = 8;
    pub const AT_ENTRY: u64 = 9;
    pub const AT_NOTELF: u64 = 10;
    pub const AT_UID: u64 = 11;
    pub const AT_EUID: u64 = 12;
    pub const AT_GID: u64 = 13;
    pub const AT_EGID: u64 = 14;
    pub const AT_RANDOM: u64 = 25;
    pub const AT_HWCAP: u64 = 16;
    pub const AT_CLKTCK: u64 = 17;
    pub const AT_HWCAP2: u64 = 26;

    pub const HWCAP_FP: u64 = 1 << 0;
    pub const HWCAP_ASIMD: u64 = 1 << 1;
    pub const HWCAP_AES: u64 = 1 << 3;
    pub const HWCAP_PMULL: u64 = 1 << 4;
    pub const HWCAP_SHA1: u64 = 1 << 5;
    pub const HWCAP_SHA2: u64 = 1 << 6;
    pub const HWCAP_CRC32: u64 = 1 << 7;
    pub const HWCAP_ATOMICS: u64 = 1 << 8;
    pub const HWCAP_FPHP: u64 = 1 << 9;
    pub const HWCAP_ASIMDHP: u64 = 1 << 10;
    pub const HWCAP_ASIMDRDM: u64 = 1 << 12;
    pub const HWCAP_JSCVT: u64 = 1 << 13;
    pub const HWCAP_FCMA: u64 = 1 << 14;
    pub const HWCAP_LRCPC: u64 = 1 << 15;
    pub const HWCAP_DCPOP: u64 = 1 << 16;
    pub const HWCAP_ASIMDDP: u64 = 1 << 20;
    pub const HWCAP_SVE: u64 = 1 << 22;

    pub const AARCH64_HWCAP: u64 =
        HWCAP_FP | HWCAP_ASIMD | HWCAP_AES | HWCAP_PMULL |
        HWCAP_SHA1 | HWCAP_SHA2 | HWCAP_CRC32 | HWCAP_ATOMICS |
        HWCAP_FPHP | HWCAP_ASIMDHP | HWCAP_ASIMDRDM |
        HWCAP_JSCVT | HWCAP_FCMA | HWCAP_LRCPC | HWCAP_DCPOP |
        HWCAP_ASIMDDP;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AuxEntry {
    pub a_type: u64,
    pub a_val: u64,
}

/// Error during ELF loading
#[derive(Debug)]
pub enum ElfError {
    InvalidFormat(&'static str),
    InvalidMagic([u8; 4]),
    WrongArchitecture,
    NotExecutable,
    DynamicallyLinked,
    OutOfMemory,
    AddressSpaceFailed,
    MappingFailed(&'static str),
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ElfError::InvalidFormat(msg) => write!(f, "Invalid ELF format: {}", msg),
            ElfError::InvalidMagic(magic) => write!(f, "Invalid ELF magic: {:02x} {:02x} {:02x} {:02x}", magic[0], magic[1], magic[2], magic[3]),
            ElfError::WrongArchitecture => write!(f, "Not an AArch64 binary"),
            ElfError::NotExecutable => write!(f, "Not an executable"),
            ElfError::DynamicallyLinked => write!(f, "Dynamically linked binary requires interpreter"),
            ElfError::OutOfMemory => write!(f, "Out of memory"),
            ElfError::AddressSpaceFailed => write!(f, "Failed to create address space"),
            ElfError::MappingFailed(msg) => write!(f, "Mapping failed: {}", msg),
        }
    }
}

pub const R_AARCH64_ABS64: u32 = 257;
pub const R_AARCH64_GLOB_DAT: u32 = 1025;
pub const R_AARCH64_JUMP_SLOT: u32 = 1026;
pub const R_AARCH64_RELATIVE: u32 = 1027;

pub const INTERP_BASE: usize = 0x3000_0000;

pub const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
pub const ELFCLASS64: u8 = 2;
pub const ELFDATA2LSB: u8 = 1;
pub const ELF64_EHDR_SIZE: usize = 64;

pub struct Elf64Ehdr {
    pub e_type: u16,
    pub e_machine: u16,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_phentsize: u16,
    pub e_phnum: u16,
}

pub struct Elf64Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
}

/// Parse a minimal Elf64_Ehdr from raw bytes.
pub fn parse_elf64_ehdr(data: &[u8]) -> Option<Elf64Ehdr> {
    if data.len() < ELF64_EHDR_SIZE { return None; }
    if data[0..4] != ELF_MAGIC { return None; }
    if data[4] != ELFCLASS64 || data[5] != ELFDATA2LSB { return None; }
    Some(Elf64Ehdr {
        e_type:     read_u16_le(data, 16),
        e_machine:  read_u16_le(data, 18),
        e_entry:    read_u64_le(data, 24),
        e_phoff:    read_u64_le(data, 32),
        e_phentsize: read_u16_le(data, 54),
        e_phnum:    read_u16_le(data, 56),
    })
}

/// Parse a single Elf64_Phdr from raw bytes.
pub fn parse_elf64_phdr(data: &[u8]) -> Option<Elf64Phdr> {
    if data.len() < 56 { return None; }
    Some(Elf64Phdr {
        p_type:  read_u32_le(data, 0),
        p_flags: read_u32_le(data, 4),
        p_offset: read_u64_le(data, 8),
        p_vaddr:  read_u64_le(data, 16),
        p_filesz: read_u64_le(data, 32),
        p_memsz:  read_u64_le(data, 40),
    })
}

pub fn read_u16_le(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

pub fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
}

pub fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;

    #[test]
    fn test_read_u16_le() {
        // Little-endian: low byte first
        let data = [0x34_u8, 0x12, 0xab, 0xcd];
        assert_eq!(read_u16_le(&data, 0), 0x1234);
        assert_eq!(read_u16_le(&data, 2), 0xcdab);
    }

    #[test]
    fn test_read_u32_le() {
        let data = [0x78_u8, 0x56, 0x34, 0x12, 0xef, 0xcd, 0xab, 0x90];
        assert_eq!(read_u32_le(&data, 0), 0x12345678);
        assert_eq!(read_u32_le(&data, 4), 0x90abcdef);
    }

    #[test]
    fn test_read_u64_le() {
        let data = [
            0xf0_u8, 0xde, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12,
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
        ];
        assert_eq!(read_u64_le(&data, 0), 0x123456789abcdef0);
        assert_eq!(read_u64_le(&data, 8), 0x8877665544332211);
    }

    fn make_valid_elf64_ehdr(e_type: u16, e_machine: u16, e_entry: u64, e_phoff: u64, e_phentsize: u16, e_phnum: u16) -> [u8; 64] {
        let mut buf = [0u8; 64];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[4] = ELFCLASS64;
        buf[5] = ELFDATA2LSB;
        buf[16..18].copy_from_slice(&e_type.to_le_bytes());
        buf[18..20].copy_from_slice(&e_machine.to_le_bytes());
        buf[24..32].copy_from_slice(&e_entry.to_le_bytes());
        buf[32..40].copy_from_slice(&e_phoff.to_le_bytes());
        buf[54..56].copy_from_slice(&e_phentsize.to_le_bytes());
        buf[56..58].copy_from_slice(&e_phnum.to_le_bytes());
        buf
    }

    #[test]
    fn test_parse_elf64_ehdr_valid() {
        // ET_EXEC = 2, EM_AARCH64 = 183, e_entry = 0x400000, e_phoff = 64, e_phentsize = 56, e_phnum = 1
        const ET_EXEC: u16 = 2;
        const EM_AARCH64: u16 = 183;
        let data = make_valid_elf64_ehdr(ET_EXEC, EM_AARCH64, 0x400000, 64, 56, 1);
        let ehdr = parse_elf64_ehdr(&data).expect("valid header should parse");
        assert_eq!(ehdr.e_type, ET_EXEC);
        assert_eq!(ehdr.e_machine, EM_AARCH64);
        assert_eq!(ehdr.e_entry, 0x400000);
        assert_eq!(ehdr.e_phoff, 64);
        assert_eq!(ehdr.e_phentsize, 56);
        assert_eq!(ehdr.e_phnum, 1);
    }

    #[test]
    fn test_parse_elf64_ehdr_invalid_magic() {
        let mut data = make_valid_elf64_ehdr(2, 183, 0x400000, 64, 56, 1);
        data[0] = 0; // corrupt magic
        assert!(parse_elf64_ehdr(&data).is_none());
    }

    #[test]
    fn test_parse_elf64_ehdr_too_short() {
        let data = [0x7f_u8, b'E', b'L', b'F', 2, 1];
        assert!(parse_elf64_ehdr(&data).is_none());
        let data63 = &make_valid_elf64_ehdr(2, 183, 0, 0, 56, 1)[..63];
        assert!(parse_elf64_ehdr(data63).is_none());
    }

    fn make_valid_elf64_phdr(p_type: u32, p_flags: u32, p_offset: u64, p_vaddr: u64, p_filesz: u64, p_memsz: u64) -> [u8; 56] {
        let mut buf = [0u8; 56];
        buf[0..4].copy_from_slice(&p_type.to_le_bytes());
        buf[4..8].copy_from_slice(&p_flags.to_le_bytes());
        buf[8..16].copy_from_slice(&p_offset.to_le_bytes());
        buf[16..24].copy_from_slice(&p_vaddr.to_le_bytes());
        buf[32..40].copy_from_slice(&p_filesz.to_le_bytes());
        buf[40..48].copy_from_slice(&p_memsz.to_le_bytes());
        buf
    }

    #[test]
    fn test_parse_elf64_phdr_valid() {
        // PT_LOAD = 1, flags 7 (RWX), offset 0, vaddr 0x400000, filesz 0x1000, memsz 0x1000
        const PT_LOAD: u32 = 1;
        let data = make_valid_elf64_phdr(PT_LOAD, 7, 0, 0x400000, 0x1000, 0x1000);
        let phdr = parse_elf64_phdr(&data).expect("valid phdr should parse");
        assert_eq!(phdr.p_type, PT_LOAD);
        assert_eq!(phdr.p_flags, 7);
        assert_eq!(phdr.p_offset, 0);
        assert_eq!(phdr.p_vaddr, 0x400000);
        assert_eq!(phdr.p_filesz, 0x1000);
        assert_eq!(phdr.p_memsz, 0x1000);
    }

    #[test]
    fn test_parse_elf64_phdr_too_short() {
        let data = [0u8; 55];
        assert!(parse_elf64_phdr(&data).is_none());
    }

    #[test]
    fn test_elf_error_display() {
        assert_eq!(
            alloc::format!("{}", ElfError::InvalidFormat("bad magic")),
            "Invalid ELF format: bad magic"
        );
        assert_eq!(
            alloc::format!("{}", ElfError::WrongArchitecture),
            "Not an AArch64 binary"
        );
        assert_eq!(
            alloc::format!("{}", ElfError::NotExecutable),
            "Not an executable"
        );
        assert_eq!(
            alloc::format!("{}", ElfError::DynamicallyLinked),
            "Dynamically linked binary requires interpreter"
        );
        assert_eq!(
            alloc::format!("{}", ElfError::OutOfMemory),
            "Out of memory"
        );
        assert_eq!(
            alloc::format!("{}", ElfError::AddressSpaceFailed),
            "Failed to create address space"
        );
        assert_eq!(
            alloc::format!("{}", ElfError::MappingFailed("page fault")),
            "Mapping failed: page fault"
        );
    }

    #[test]
    fn test_constants() {
        assert_eq!(ELF_MAGIC, [0x7f, b'E', b'L', b'F']);
        assert_eq!(ELFCLASS64, 2);
        assert_eq!(ELFDATA2LSB, 1);
        assert_eq!(ELF64_EHDR_SIZE, 64);
    }
}
