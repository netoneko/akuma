//! vCPU argument structures. Sizes and field positions are from the dumps the VMM sent at the
//! first `RUN` (PVH entry: 32-bit flat protected mode, `rip=0x201000`, `rbx=0x6000`).

use crate::{rd32, rd64, wr32, wr64};

/// `SET_REGS`: eighteen 64-bit registers.
pub const REGS_LEN: usize = 144;
/// `GET/SET_SREGS`.
pub const SREGS_LEN: usize = 312;
/// A CPUID entry: leaf, subleaf, flags, eax..edx, three words of padding.
pub const CPUID_ENTRY_LEN: usize = 40;
/// An MSR entry: index, pad, data.
pub const MSR_ENTRY_LEN: usize = 16;
/// Header before the entries of `SET_CPUID2` / `GET_SUPPORTED_CPUID` / `SET_MSRS`: count + padding.
pub const LIST_HDR: usize = 8;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Regs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

impl Regs {
    fn words(&self) -> [u64; 18] {
        [
            self.rax, self.rbx, self.rcx, self.rdx, self.rsi, self.rdi, self.rsp, self.rbp, self.r8, self.r9,
            self.r10, self.r11, self.r12, self.r13, self.r14, self.r15, self.rip, self.rflags,
        ]
    }

    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        let w = |i: usize| rd64(b, i * 8);
        Some(Self {
            rax: w(0)?,
            rbx: w(1)?,
            rcx: w(2)?,
            rdx: w(3)?,
            rsi: w(4)?,
            rdi: w(5)?,
            rsp: w(6)?,
            rbp: w(7)?,
            r8: w(8)?,
            r9: w(9)?,
            r10: w(10)?,
            r11: w(11)?,
            r12: w(12)?,
            r13: w(13)?,
            r14: w(14)?,
            r15: w(15)?,
            rip: w(16)?,
            rflags: w(17)?,
        })
    }

    pub fn encode(&self, out: &mut [u8; REGS_LEN]) {
        for (i, v) in self.words().into_iter().enumerate() {
            wr64(out, i * 8, v);
        }
    }
}

/// One segment register as the VMM states it (24 bytes): base, limit, selector, type, present, dpl,
/// db, s, l, g, avl, unusable, padding.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub ty: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    pub unusable: u8,
}

impl Segment {
    const LEN: usize = 24;

    fn parse(b: &[u8]) -> Option<Self> {
        let g = |i: usize| b.get(i).copied();
        Some(Self {
            base: rd64(b, 0)?,
            limit: rd32(b, 8)?,
            selector: u16::from_le_bytes([g(12)?, g(13)?]),
            ty: g(14)?,
            present: g(15)?,
            dpl: g(16)?,
            db: g(17)?,
            s: g(18)?,
            l: g(19)?,
            g: g(20)?,
            avl: g(21)?,
            unusable: g(22)?,
        })
    }

    fn encode(&self, b: &mut [u8]) {
        wr64(b, 0, self.base);
        wr32(b, 8, self.limit);
        b[12..14].copy_from_slice(&self.selector.to_le_bytes());
        b[14..23].copy_from_slice(&[self.ty, self.present, self.dpl, self.db, self.s, self.l, self.g, self.avl, self.unusable]);
        b[23] = 0;
    }

    /// The VMCB segment attribute word (APM Vol 2 §15.5.1 / Fig B-2): type[3:0], S[4], DPL[6:5], P[7],
    /// AVL[8], L[9], D/B[10], G[11].
    #[must_use]
    pub fn vmcb_attrib(&self) -> u16 {
        if self.unusable != 0 {
            return 0;
        }
        u16::from(self.ty & 0xf)
            | u16::from(self.s & 1) << 4
            | u16::from(self.dpl & 3) << 5
            | u16::from(self.present & 1) << 7
            | u16::from(self.avl & 1) << 8
            | u16::from(self.l & 1) << 9
            | u16::from(self.db & 1) << 10
            | u16::from(self.g & 1) << 11
    }

    /// The inverse of [`Segment::vmcb_attrib`]; `limit` is the byte-scaled value the VMCB holds.
    #[must_use]
    pub fn from_vmcb(base: u64, limit: u32, selector: u16, attrib: u16) -> Self {
        Self {
            base,
            limit,
            selector,
            ty: (attrib & 0xf) as u8,
            s: ((attrib >> 4) & 1) as u8,
            dpl: ((attrib >> 5) & 3) as u8,
            present: ((attrib >> 7) & 1) as u8,
            avl: ((attrib >> 8) & 1) as u8,
            l: ((attrib >> 9) & 1) as u8,
            db: ((attrib >> 10) & 1) as u8,
            g: ((attrib >> 11) & 1) as u8,
            unusable: u8::from(attrib & 0x80 == 0),
        }
    }
}

/// A descriptor-table register: base and limit (16 bytes with padding).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Dtable {
    pub base: u64,
    pub limit: u16,
}

/// `GET/SET_SREGS`: eight segments (cs ds es fs gs ss tr ldt), gdt, idt, control registers, EFER,
/// APIC base, and a 256-bit pending-interrupt bitmap.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Sregs {
    pub cs: Segment,
    pub ds: Segment,
    pub es: Segment,
    pub fs: Segment,
    pub gs: Segment,
    pub ss: Segment,
    pub tr: Segment,
    pub ldt: Segment,
    pub gdt: Dtable,
    pub idt: Dtable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub apic_base: u64,
    pub interrupt_bitmap: [u64; 4],
}

impl Sregs {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < SREGS_LEN {
            return None;
        }
        let seg = |i: usize| Segment::parse(&b[i * Segment::LEN..]);
        let dt = |o: usize| Some(Dtable { base: rd64(b, o)?, limit: u16::from_le_bytes([b[o + 8], b[o + 9]]) });
        Some(Self {
            cs: seg(0)?,
            ds: seg(1)?,
            es: seg(2)?,
            fs: seg(3)?,
            gs: seg(4)?,
            ss: seg(5)?,
            tr: seg(6)?,
            ldt: seg(7)?,
            gdt: dt(192)?,
            idt: dt(208)?,
            cr0: rd64(b, 224)?,
            cr2: rd64(b, 232)?,
            cr3: rd64(b, 240)?,
            cr4: rd64(b, 248)?,
            cr8: rd64(b, 256)?,
            efer: rd64(b, 264)?,
            apic_base: rd64(b, 272)?,
            interrupt_bitmap: [rd64(b, 280)?, rd64(b, 288)?, rd64(b, 296)?, rd64(b, 304)?],
        })
    }

    pub fn encode(&self, out: &mut [u8; SREGS_LEN]) {
        for (i, s) in [&self.cs, &self.ds, &self.es, &self.fs, &self.gs, &self.ss, &self.tr, &self.ldt].into_iter().enumerate() {
            s.encode(&mut out[i * Segment::LEN..(i + 1) * Segment::LEN]);
        }
        for (o, d) in [(192, &self.gdt), (208, &self.idt)] {
            wr64(out, o, d.base);
            out[o + 8..o + 10].copy_from_slice(&d.limit.to_le_bytes());
            out[o + 10..o + 16].fill(0);
        }
        for (i, v) in [self.cr0, self.cr2, self.cr3, self.cr4, self.cr8, self.efer, self.apic_base].into_iter().enumerate() {
            wr64(out, 224 + i * 8, v);
        }
        for (i, v) in self.interrupt_bitmap.into_iter().enumerate() {
            wr64(out, 280 + i * 8, v);
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CpuidEntry {
    pub function: u32,
    pub index: u32,
    pub flags: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

impl CpuidEntry {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            function: rd32(b, 0)?,
            index: rd32(b, 4)?,
            flags: rd32(b, 8)?,
            eax: rd32(b, 12)?,
            ebx: rd32(b, 16)?,
            ecx: rd32(b, 20)?,
            edx: rd32(b, 24)?,
        })
    }

    pub fn encode(&self, out: &mut [u8; CPUID_ENTRY_LEN]) {
        out.fill(0);
        for (i, v) in [self.function, self.index, self.flags, self.eax, self.ebx, self.ecx, self.edx].into_iter().enumerate() {
            wr32(out, i * 4, v);
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MsrEntry {
    pub index: u32,
    pub data: u64,
}

impl MsrEntry {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self { index: rd32(b, 0)?, data: rd64(b, 8)? })
    }

    pub fn encode(&self, out: &mut [u8; MSR_ENTRY_LEN]) {
        out.fill(0);
        wr32(out, 0, self.index);
        wr64(out, 8, self.data);
    }
}

/// The element count in a list header, if the whole list (`n` entries of `stride`) fits in `len`
/// bytes of argument. The kernel never reads past what the VMM said it owns.
#[must_use]
pub fn list_count(hdr: &[u8], stride: usize, len: usize) -> Option<usize> {
    let n = rd32(hdr, 0)? as usize;
    (LIST_HDR + n.checked_mul(stride)? <= len).then_some(n)
}
