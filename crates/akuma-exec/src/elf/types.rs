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
pub const ELF64_EHDR_SIZE: usize = 64;

// The hand-rolled ELF64 header/phdr parser that used to live here — `Elf64Ehdr`,
// `Elf64Phdr`, `parse_elf64_phdr` and the unchecked `read_u{16,32,64}_le`
// helpers reading fields at literal spec offsets — is gone. Both loaders now
// parse through the `elf` 0.7 crate (`super::source`), which is bounds-checked
// throughout and gets third-party scrutiny. The two parsers were verified to
// agree on all 2,387 ELF files in the tree, with zero panics across 280 hostile
// header-field mutations, before this deletion:
// `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §3.

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;

    // Header/phdr parsing is covered by `super::source`'s tests now — the
    // hand-rolled parser those used to exercise no longer exists.

    #[test]
    fn test_elf_error_display() {
        assert_eq!(
            alloc::format!("{}", ElfError::InvalidFormat("bad magic")),
            "Invalid ELF format: bad magic"
        );
        assert_eq!(
            alloc::format!("{}", ElfError::InvalidMagic([0x23, 0x21, 0x2f, 0x62])),
            "Invalid ELF magic: 23 21 2f 62"
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
        assert_eq!(ELF64_EHDR_SIZE, 64);
        assert_eq!(INTERP_BASE, 0x3000_0000);
    }
}
