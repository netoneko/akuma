#![no_std]
#![forbid(unsafe_code)]
//! The amd64 page-table entry, as data.
//!
//! # Why this is a crate
//!
//! Two walkers on this target read and write the same 64-bit entries —
//! `amd64/src/paging.rs` for the kernel's own mappings and `akuma-mmu`'s
//! `UserAddressSpace` for every process's — and each carried its own spelling of
//! the bits (`P`/`RW`/`US` in one, `X86_P`/`X86_RW`/`X86_US` in the other), its
//! own `index`, and its own copy of the rule for stepping through an
//! intermediate entry. A boot self-test used to pin the two copies against each
//! other (`x86_prot_matches_amd64_encoding`). There is one copy now.
//!
//! What it deliberately does **not** do is touch memory. Reading and writing a
//! table is `akuma-pagetable`'s, which is where the `unsafe` is; everything here
//! is a function of a `u64`, so it is `#![forbid(unsafe_code)]` and every
//! property below is a host test.
//!
//! # The two vocabularies
//!
//! [`PteProt`] is what the **hardware** is told; `akuma_mmap::Prot` is what a
//! **region** is supposed to be. [`PteProt::from_region`] is the only bridge.
//! The AArch64 encoding shares no field with this one:
//!
//! | | AArch64 | amd64 |
//! |---|---|---|
//! | writable | `AP[7:6]` == `01` (a *field*) | bit 1 |
//! | user | `AP[6]` | bit 2 |
//! | no-execute (user) | `UXN`, bit 54 | `NX`, bit 63 |
//! | no-execute (kernel) | `PXN`, bit 53 | — (no separate bit) |
//! | access flag | `AF`, bit 10, software-set | `A`, bit 5, hardware-set |
//!
//! `PXN` has no amd64 counterpart, which is why `RO` and `RX` collapse here.

use akuma_mmap::Prot;

/// Present.
pub const P: u64 = 1 << 0;
/// Writable.
pub const RW: u64 = 1 << 1;
/// User-accessible (ring 3).
pub const US: u64 = 1 << 2;
/// Page-level write-through.
pub const PWT: u64 = 1 << 3;
/// Page-level cache disable.
pub const PCD: u64 = 1 << 4;
/// Page size — at PD level a 2 MiB page rather than a PT pointer.
pub const PS: u64 = 1 << 7;
/// Bit 7 of a **4 KiB leaf** is the PAT selector, not `PS` — the same position
/// means "large page" one level up. Only ever written into a level-1 entry, by
/// [`MemAttr::WriteCombine`].
pub const PAT_4K: u64 = 1 << 7;
/// Software bit 9, the copy-on-write marker. The CPU ignores bits 9..11 of a
/// present leaf entirely.
///
/// **A marker is required, not a convenience.** A CoW-demoted page and one that
/// is read-only *on purpose* (`mprotect(PROT_READ)`, an ELF `.rodata` segment)
/// are byte-identical otherwise, and this target has no region table to consult
/// instead. Deciding from the share count alone promotes an `mprotect`ed page to
/// writable the moment its frame happens to be shared — the trap
/// `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` is about.
pub const COW: u64 = 1 << 9;
/// No-execute. **Requires `EFER.NXE`**, which `boot.s` sets alongside `LME`;
/// without it this is a reserved bit and setting it faults — on the first touch
/// of a non-executable page, not when it is mapped.
pub const NX: u64 = 1 << 63;
/// Physical address field of an entry: bits 51:12.
pub const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
/// The cache-type bits of a 4 KiB leaf: what a re-protect must carry across.
pub const CACHE_MASK: u64 = PWT | PCD | PAT_4K;

/// Bytes in a page.
pub const PAGE_SIZE: usize = 4096;

/// A page's permissions, as the hardware is told them.
///
/// A struct rather than a `u64` so `write`/`exec`/`user` cannot collide the way
/// AArch64's `RO`/`EXEC` constants alias.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PteProt {
    pub write: bool,
    pub exec: bool,
    pub user: bool,
}

impl PteProt {
    /// User read/write, no execute. The default for data.
    pub const USER_RW: Self = Self { write: true, exec: false, user: true };
    /// User read + execute, not writable.
    pub const USER_RX: Self = Self { write: false, exec: true, user: true };
    /// User read, write **and** execute: a JIT's page (V8 asks for exactly this
    /// on its code range). The only user shape with `NX` clear and `R/W` set;
    /// nothing in the kernel's own tables uses it.
    pub const USER_RWX: Self = Self { write: true, exec: true, user: true };
    /// User read only — no write, no execute.
    pub const USER_RO: Self = Self { write: false, exec: false, user: true };
    /// Kernel read/write, no execute. The default for the kernel's own data.
    pub const KERNEL_RW: Self = Self { write: true, exec: false, user: false };
    /// Kernel read + execute, not writable. The only executable kernel shape
    /// offered: there is deliberately no writable-and-executable **kernel**
    /// constructor.
    pub const KERNEL_RX: Self = Self { write: false, exec: true, user: false };
    /// Kernel read-only, no execute. What a `PROT_NONE` user page becomes —
    /// amd64 has no "present and wholly inaccessible" encoding, so clearing
    /// `U/S` is what makes a ring-3 touch fault.
    pub const KERNEL_RO: Self = Self { write: false, exec: false, user: false };

    /// The amd64 spelling of a neutral [`Prot`].
    ///
    /// Two divergences from AArch64, both deliberate:
    ///
    /// * `RO` and `RX` collapse. They differ only in `PXN` — whether EL1 may
    ///   fetch — and amd64 has one execute bit.
    /// * `RW` is writable **and** executable ([`Self::USER_RWX`]), as on AArch64
    ///   and Linux: V8 `mprotect`s its whole code range RWX and every Chromium
    ///   renderer died on the refusal (2026-10-08; `jitprobe.c` is the gate).
    #[must_use]
    pub const fn from_region(prot: Prot) -> Self {
        match prot.tag() {
            0 => Self::KERNEL_RO,
            1 | 4 => Self::USER_RX,
            2 => Self::USER_RWX,
            3 => Self::USER_RW,
            5 => Self::USER_RO,
            // Unreachable over `Prot::ALL`. A `const fn` cannot panic out of a
            // `u8` match, so the fallback is **fail-closed**: a seventh variant
            // maps to a page ring 3 cannot touch, which faults visibly instead
            // of over-granting.
            _ => Self::KERNEL_RO,
        }
    }
}

/// How a mapping is cached — the other half of [`encode`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MemAttr {
    /// Normal RAM: writeback cached.
    WriteBack,
    /// Device MMIO: uncacheable, never speculatively read. A writeback-cached
    /// MMIO register can be answered from cache and the access never issued.
    Device,
    /// Write-combining, for a framebuffer: stores gather in the WC buffers and
    /// leave as full-line bursts; reads are uncached. Encoded as the 4 KiB PTE's
    /// **PAT bit** with `PWT = PCD = 0`, which selects PAT entry 4 — the entry
    /// `multiboot2::map_wc` programs to WC on every core. Where that never
    /// happened entry 4 is the firmware's WB, which under the framebuffer's UC
    /// MTRR is UC: slow, never wrong.
    WriteCombine,
}

/// Encode permissions and cacheability into PTE bits, with the CoW marker clear.
#[must_use]
pub const fn encode(prot: PteProt, attr: MemAttr) -> u64 {
    let mut bits = P;
    if prot.write {
        bits |= RW;
    }
    if prot.user {
        bits |= US;
    }
    if !prot.exec {
        bits |= NX;
    }
    match attr {
        MemAttr::WriteBack => {}
        MemAttr::Device => bits |= PCD | PWT,
        MemAttr::WriteCombine => bits |= PAT_4K,
    }
    bits
}

/// [`encode`] plus the copy-on-write marker.
///
/// `cow` is a separate argument rather than a field of [`PteProt`] because it is
/// not a permission: it is a software marker the hardware ignores.
#[must_use]
pub const fn encode_pte(prot: PteProt, attr: MemAttr, cow: bool) -> u64 {
    encode(prot, attr) | if cow { COW } else { 0 }
}

/// The [`MemAttr`] a 4 KiB leaf's cache bits mean.
///
/// The inverse of the `attr` half of [`encode`] for the three shapes it writes. Any other combination (a
/// PWT-only or PCD-only entry, which nothing here writes) reads as `Device`: not
/// RAM, so not something a caller may refcount.
#[must_use]
pub const fn decode_attr(entry: u64) -> MemAttr {
    match entry & CACHE_MASK {
        0 => MemAttr::WriteBack,
        PAT_4K => MemAttr::WriteCombine,
        _ => MemAttr::Device,
    }
}

/// The permission triple a present leaf carries.
#[must_use]
pub const fn decode_prot(entry: u64) -> PteProt {
    PteProt { write: entry & RW != 0, exec: entry & NX == 0, user: entry & US != 0 }
}

/// Is the present bit set?
#[must_use]
pub const fn is_present(entry: u64) -> bool {
    entry & P != 0
}

/// Does a present entry end the walk early as a large page? Only meaningful at
/// levels 2 and 3; at level 1 the same bit is [`PAT_4K`].
#[must_use]
pub const fn is_large(entry: u64) -> bool {
    entry & PS != 0
}

/// Is the CoW marker set?
#[must_use]
pub const fn is_cow(entry: u64) -> bool {
    entry & COW != 0
}

/// The physical frame an entry points at. For a large-page entry bit 12 is the
/// PAT bit, not an address bit; mask with [`large_frame`] instead.
#[must_use]
pub const fn frame(entry: u64) -> u64 {
    entry & ADDR_MASK
}

/// The base address of a large-page entry found at `level` (3 = 1 GiB, 2 = 2 MiB):
/// `frame` with the PAT bit (bit 12) and the sub-page address bits cleared.
#[must_use]
pub const fn large_frame(entry: u64, level: u32) -> u64 {
    entry & ADDR_MASK & !(entry_span(level) as u64 - 1)
}

/// Bytes one entry at `level` spans: 512 GiB, 1 GiB, 2 MiB, 4 KiB for levels
/// 4, 3, 2, 1.
#[must_use]
pub const fn entry_span(level: u32) -> usize {
    1usize << (12 + 9 * (level - 1))
}

/// A leaf entry mapping `pa` — `encode_pte` with the frame folded in.
#[must_use]
pub const fn leaf(pa: u64, prot: PteProt, attr: MemAttr, cow: bool) -> u64 {
    (pa & ADDR_MASK) | encode_pte(prot, attr, cow)
}

/// An intermediate entry pointing at the table in `frame`. Permissive: the leaf
/// decides, and `NX` is left clear for the same reason.
#[must_use]
pub const fn table_entry(frame: u64, user: bool) -> u64 {
    frame | P | RW | if user { US } else { 0 }
}

/// What stepping down through one intermediate entry found.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Not present: the caller allocates a table and stores
    /// `table_entry(frame, user)`.
    Absent,
    /// A large page, not a table pointer. Splitting is not implemented, so a
    /// mapper refuses; `boot.s`'s identity map is built from 2 MiB pages, and an
    /// attempt to map a 4 KiB page below 1 GiB lands here instead of silently
    /// corrupting the map.
    Large,
    /// A table at `frame`. `widened` is `Some(new_entry)` when the entry must be
    /// rewritten first: a parent that is not user-accessible or not writable
    /// masks every child, so an intermediate entry has to be at least as
    /// permissive as any leaf beneath it. Enforcement lives entirely in the leaf.
    Table { frame: u64, widened: Option<u64> },
}

/// Classify an intermediate entry for a **mapping** descent that needs the path
/// to be user-accessible iff `user`.
#[must_use]
pub const fn step_down(entry: u64, user: bool) -> Step {
    if !is_present(entry) {
        return Step::Absent;
    }
    if is_large(entry) {
        return Step::Large;
    }
    let want = P | RW | if user { US } else { 0 };
    let widened = if entry & want != want { Some((entry | want) & !NX) } else { None };
    Step::Table { frame: frame(entry), widened }
}

/// The error code the CPU pushes for vector 14, asked as questions.
///
/// It was five `const`s declared **inside** `idt::page_fault_dispatch`, with
/// `code & 1 == 0` written out separately for the not-present test. The bits had
/// no owner, two of the five (reserved-bit, instruction-fetch) were never named,
/// and nothing pinned the positions. Positions are Intel SDM Vol. 3A §4.7; bit 4
/// is only ever set when `EFER.NXE` is on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PageFaultCode(u64);

impl PageFaultCode {
    const P_BIT: u64 = 1 << 0;
    const WR_BIT: u64 = 1 << 1;
    const US_BIT: u64 = 1 << 2;
    const RSVD_BIT: u64 = 1 << 3;
    const ID_BIT: u64 = 1 << 4;

    /// Wrap the raw code the CPU pushed.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw code, for the diagnostic printers.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The page was **not present**: nothing was mapped at the faulting address.
    ///
    /// The demand-paging question. Its complement is [`Self::protection`];
    /// servicing a *protection* fault by mapping a fresh page would silently
    /// defeat whatever the protection was for.
    #[must_use]
    pub const fn not_present(self) -> bool {
        self.0 & Self::P_BIT == 0
    }

    /// A present page was accessed in a way its permissions forbid.
    #[must_use]
    pub const fn protection(self) -> bool {
        !self.not_present()
    }

    /// The faulting access was a write.
    #[must_use]
    pub const fn is_write(self) -> bool {
        self.0 & Self::WR_BIT != 0
    }

    /// The faulting access came from **ring 3**. This is the CPL of the access,
    /// not the `U/S` bit of the page: a `copy_to_user` writing a user page from
    /// ring 0 answers `false`.
    #[must_use]
    pub const fn is_user_mode(self) -> bool {
        self.0 & Self::US_BIT != 0
    }

    /// A paging-structure entry had a reserved bit set. Never a userspace fault
    /// and never fixable: this kernel's own walker wrote a malformed entry (the
    /// usual cause is [`NX`] with `EFER.NXE` clear).
    #[must_use]
    pub const fn reserved_bit(self) -> bool {
        self.0 & Self::RSVD_BIT != 0
    }

    /// The faulting access was an instruction fetch, not a data access.
    #[must_use]
    pub const fn instruction_fetch(self) -> bool {
        self.0 & Self::ID_BIT != 0
    }

    /// The shape a copy-on-write break answers to: a write to a page that **is**
    /// mapped, from either ring.
    ///
    /// Ring 3 is *not* part of it. It was, until 2026-09-07, and that was wrong
    /// in a way nothing could observe while `CR0.WP` was clear: `copy_to_user`
    /// writes a user page **from ring 0**, so a `read(2)` into a forked child's
    /// untouched buffer arrives with `U/S` clear, and requiring ring 3 sent it to
    /// the user-copy fixup as an unexplained `EFAULT`. Letting a supervisor write
    /// through costs nothing: the break re-checks that the page is
    /// user-accessible, and `akuma_cow` refuses a page that is read-only on
    /// purpose.
    #[must_use]
    pub const fn is_write_to_present_page(self) -> bool {
        self.protection() && self.is_write()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literals, not expressions over the constants: an expression re-derives the
    /// answer from the same constants the code under test uses, so it agrees with
    /// a typo. These are the bits, written out.
    #[test]
    fn named_shapes_encode_to_pinned_bits() {
        let wb = |p| encode(p, MemAttr::WriteBack);
        assert_eq!(wb(PteProt::KERNEL_RO), 0x8000_0000_0000_0001);
        assert_eq!(wb(PteProt::KERNEL_RW), 0x8000_0000_0000_0003);
        assert_eq!(wb(PteProt::KERNEL_RX), 0x0000_0000_0000_0001);
        assert_eq!(wb(PteProt::USER_RW), 0x8000_0000_0000_0007);
        assert_eq!(wb(PteProt::USER_RX), 0x0000_0000_0000_0005);
        assert_eq!(wb(PteProt::USER_RO), 0x8000_0000_0000_0005);
        assert_eq!(wb(PteProt::USER_RWX), 0x0000_0000_0000_0007);
        // The CoW demotion: write cleared, bit 9 set — both halves in one value.
        assert_eq!(encode_pte(PteProt::USER_RO, MemAttr::WriteBack, true), 0x8000_0000_0000_0205);
        assert_eq!(
            encode_pte(PteProt::USER_RO, MemAttr::WriteBack, true) ^ wb(PteProt::USER_RO),
            COW
        );
        // Device adds PCD|PWT — the LAPIC depends on it.
        assert_eq!(encode(PteProt::KERNEL_RW, MemAttr::Device), 0x8000_0000_0000_001b);
    }

    #[test]
    fn region_vocabulary_is_pinned_variant_by_variant() {
        let wb = |p| encode(PteProt::from_region(p), MemAttr::WriteBack);
        assert_eq!(wb(Prot::NONE), 0x8000_0000_0000_0001, "present, kernel-only: ring 3 faults");
        assert_eq!(wb(Prot::RO), 0x0000_0000_0000_0005);
        assert_eq!(wb(Prot::RX), 0x0000_0000_0000_0005, "RO and RX collapse: no PXN");
        assert_eq!(wb(Prot::RW), 0x0000_0000_0000_0007, "RW is RWX since 2026-10-08");
        assert_eq!(wb(Prot::RW_NO_EXEC), 0x8000_0000_0000_0007);
        assert_eq!(wb(Prot::RO_NO_EXEC), 0x8000_0000_0000_0005);
    }

    /// `from_region` matches on an opaque tag so cannot be exhaustive: prove the
    /// `_ =>` fallback is unreachable over `Prot::ALL` and the one collapse is
    /// the only one.
    #[test]
    fn from_region_covers_every_variant() {
        let mut seen = [PteProt::KERNEL_RO; 6];
        let mut distinct = 0;
        for p in Prot::ALL {
            let e = PteProt::from_region(p);
            if !seen[..distinct].contains(&e) {
                seen[distinct] = e;
                distinct += 1;
            }
        }
        assert_eq!(distinct, 5, "the set of encodings changed");
        assert!(!PteProt::from_region(Prot::NONE).user, "NONE must not become user-reachable");
        assert_eq!(Prot::ALL.len(), 6, "a seventh variant lands on the fail-closed arm");
    }

    #[test]
    fn write_xor_execute_is_expressible_only_where_asked() {
        let rw = encode(PteProt::KERNEL_RW, MemAttr::WriteBack);
        let rx = encode(PteProt::KERNEL_RX, MemAttr::WriteBack);
        let ro = encode(PteProt::KERNEL_RO, MemAttr::WriteBack);
        assert!(rw & NX != 0 && rw & RW != 0, "kernel data is non-executable");
        assert!(rx & NX == 0 && rx & RW == 0, "kernel code is exec and not writable");
        assert!(ro & NX != 0 && ro & RW == 0);
        assert_ne!(ro, rx, "read-only and executable differ");
        assert_eq!((rw | rx | ro) & US, 0, "kernel mappings are not user-reachable");
    }

    #[test]
    fn cache_attrs_round_trip_and_cow_is_disjoint() {
        let rw = PteProt::from_region(Prot::RW);
        for a in [MemAttr::WriteBack, MemAttr::Device, MemAttr::WriteCombine] {
            assert_eq!(decode_attr(encode(rw, a) | 0x1234_5000), a, "{a:?}");
        }
        let wc = encode(rw, MemAttr::WriteCombine);
        assert_eq!(wc & CACHE_MASK, PAT_4K);
        assert_eq!(PAT_4K, 1 << 7);
        assert_eq!(wc & !CACHE_MASK, encode(rw, MemAttr::WriteBack), "only the cache bits differ");
        assert_eq!(COW, 1 << 9);
        for p in Prot::ALL {
            let e = encode(PteProt::from_region(p), MemAttr::WriteBack);
            assert_eq!(e & COW, 0, "{p:?} encodes into the CoW marker bit");
        }
        assert_eq!(wc & COW, 0, "the PAT bit is not the CoW marker");
    }

    #[test]
    fn decode_prot_inverts_encode() {
        for p in [
            PteProt::USER_RW,
            PteProt::USER_RX,
            PteProt::USER_RWX,
            PteProt::USER_RO,
            PteProt::KERNEL_RW,
            PteProt::KERNEL_RX,
            PteProt::KERNEL_RO,
        ] {
            let e = leaf(0x1234_5000, p, MemAttr::WriteBack, true);
            assert_eq!(decode_prot(e), p);
            assert!(is_cow(e));
            assert_eq!(frame(e), 0x1234_5000);
        }
    }

    #[test]
    fn step_down_classifies_and_widens() {
        assert_eq!(step_down(0, true), Step::Absent);
        assert_eq!(step_down(0x1000 | P | PS, true), Step::Large);
        // Already permissive enough: no rewrite.
        let e = table_entry(0x5000, true);
        assert_eq!(step_down(e, true), Step::Table { frame: 0x5000, widened: None });
        // A kernel-only parent beneath a user mapping gains US|RW, loses NX.
        let kernel = 0x5000 | P | NX;
        assert_eq!(
            step_down(kernel, true),
            Step::Table { frame: 0x5000, widened: Some(0x5000 | P | RW | US) }
        );
        // A kernel mapping does not demand US.
        assert_eq!(step_down(table_entry(0x5000, false), false), Step::Table { frame: 0x5000, widened: None });
        assert_eq!(table_entry(0x5000, false) & NX, 0, "intermediate entries leave NX clear");
    }

    #[test]
    fn large_frame_drops_the_pat_bit() {
        // Bit 12 of a PDE is PAT (multiboot2::map_wc sets it), not an address bit.
        let pde = 0x4000_0000 | 0x1000 | P | PS;
        assert_eq!(large_frame(pde, 2), 0x4000_0000);
        assert_eq!(frame(pde), 0x4000_1000);
        assert_eq!(large_frame(0x8000_0000 | P | PS, 3), 0x8000_0000, "a 1 GiB page");
    }

    #[test]
    fn spans() {
        assert_eq!(entry_span(1), 4096);
        assert_eq!(entry_span(2), 2 << 20);
        assert_eq!(entry_span(3), 1 << 30);
        assert_eq!(entry_span(4), 512 << 30);
    }

    /// Values are Intel SDM Vol. 3A §4.7.
    #[test]
    fn page_fault_code_bits() {
        // Bit 0 clear is "not present" — the inverted sense. Getting it backwards
        // services a protection fault by mapping a fresh page over it.
        assert!(PageFaultCode::new(0).not_present());
        assert!(PageFaultCode::new(0b0_0001).protection());
        assert!(PageFaultCode::new(0b0_0010).is_write());
        assert!(PageFaultCode::new(0b0_0100).is_user_mode());
        assert!(PageFaultCode::new(0b0_1000).reserved_bit());
        assert!(PageFaultCode::new(0b1_0000).instruction_fetch());
        // Each predicate reads only its own bit.
        assert!(!PageFaultCode::new(0b1_1101).is_write());
        assert!(!PageFaultCode::new(0b1_1011).is_user_mode());
        assert!(!PageFaultCode::new(0b0_1111).instruction_fetch());
        // The CoW shape: a write to a present page, from either ring.
        assert!(PageFaultCode::new(0b0_0111).is_write_to_present_page());
        assert!(PageFaultCode::new(0b0_0011).is_write_to_present_page());
        for raw in [0b0_0110u64, 0b0_0101] {
            assert!(!PageFaultCode::new(raw).is_write_to_present_page(), "{raw:#b}");
        }
        assert_eq!(PageFaultCode::new(0x1f).raw(), 0x1f);
    }
}
