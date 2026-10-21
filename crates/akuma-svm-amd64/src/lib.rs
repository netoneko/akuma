#![no_std]
#![forbid(unsafe_code)]
//! The AMD SVM (AMD-V) VMCB, as data.
//!
//! # Why this is a crate
//!
//! A hypervisor's correctness lives in wire-layout constants: one transposed
//! offset and the hardware reads the wrong register *silently*, because the
//! VMCB is untyped bytes to the CPU. Every constant below is transcribed from
//! the AMD64 Architecture Programmer's Manual Vol 2 (24593) rev 3.45 and
//! pinned by a host test that cites its table, so a wrong from-memory value
//! fails a test instead of spending a metal boot. The spec-first pass caught
//! three such values already (EFER at save-relative `0D0h` not `080h`; three
//! intercept dwords, not one; IOPM base at `040h` not `014h`).
//!
//! The layering is the tree's usual one: this crate owns the **vocabulary**
//! (offsets, bits, exit codes, segment attributes, the MSR and I/O permission
//! maps) and the **decisions** that
//! are pure functions of a VMCB's bytes — the VMRUN consistency checks of
//! §15.5.1, which the hardware answers with `VMEXIT_INVALID` and no
//! diagnostics. `amd64/src/kvm.rs` owns every `unsafe` (the two MSR writes
//! and the `VMRUN` itself); it calls [`Vmcb`] over an already zero-filled
//! page and never names a field offset of its own.
//!
//! # The one obligation the crate hands its caller
//!
//! **Unused VMCB bytes must be zero** (Table B-1 note), and the control area
//! is padded to `0400h` where the state save area begins. Construct a VMCB on
//! a zero-filled page (`FreshFrame::alloc_zeroed`) and only ever write typed
//! fields; a stale or recycled page is a guest state nobody validated.
//!
//! # Sources
//!
//! AMD APM Vol 2 (24593) rev 3.45, July 2026: Table B-1 (control area),
//! Table B-2 (state save area), §15.4 (enabling), §15.5.1 (VMRUN
//! consistency), §15.30.4 (VM_HSAVE_PA), §15.10.1 (IOPM), §15.11 (MSRPM),
//! Appendix C (exit codes). Offsets in
//! [`save`] are **relative to the save-area start** (`0400h` in the page);
//! [`ctrl`] offsets are from the page base.

/// Page size of a VMCB (and of the host save area).
pub const VMCB_SIZE: usize = 4096;
/// The state save area begins here in the VMCB page (Table B-2; SEV-ES off).
pub const SAVE_AREA_BASE: usize = 0x400;

/// SVM MSRs and their bit vocabulary (§15.4, §15.30.4). The *writes* happen
/// in `akuma_cpu::msr` at the kernel call site; this is the data.
pub mod msr {
    /// `VM_CR` — SVM lock/enable controls.
    pub const VM_CR: u32 = 0xC001_0114;
    /// `VM_CR.SVMDIS` (bit 4): set means SVM is disabled; with
    /// `Fn8000_000A.EDX[SVML]` clear it is firmware-locked and cannot be
    /// unlocked by software.
    pub const VM_CR_SVMDIS: u64 = 1 << 4;
    /// `EFER` address (same register the kernel already touches for `SCE`).
    pub const EFER: u32 = 0xC000_0080;
    /// `EFER.SVME` (bit 12) — enables `VMRUN`.
    pub const EFER_SVME: u64 = 1 << 12;
    /// `EFER.LME` (bit 8) — long mode enable (guest state).
    pub const EFER_LME: u64 = 1 << 8;
    /// `EFER.LMA` (bit 10) — long mode active (guest state).
    pub const EFER_LMA: u64 = 1 << 10;
    /// `VM_HSAVE_PA` — host save area base (§15.30.4): 4 KiB-aligned
    /// physical address, low 12 bits clear, never the VMCB page.
    pub const VM_HSAVE_PA: u32 = 0xC001_0117;
}

/// Control-area offsets, from the VMCB page base (Table B-1).
pub mod ctrl {
    /// CR intercepts: 15:0 reads CR0–15, 31:16 writes.
    pub const CR_INTERCEPT: usize = 0x000;
    /// DR intercepts: same split as [`CR_INTERCEPT`].
    pub const DR_INTERCEPT: usize = 0x004;
    /// Exception intercepts: bit = vector, 0–31.
    pub const EXCP_INTERCEPT: usize = 0x008;
    /// Instruction intercept dword 0 (bits in [`intr0`]).
    pub const INSTR_INTERCEPT0: usize = 0x00C;
    /// Instruction intercept dword 1 (bits in [`intr1`]).
    pub const INSTR_INTERCEPT1: usize = 0x010;
    /// Instruction intercept dword 2 (bits in [`intr2`]).
    pub const INSTR_INTERCEPT2: usize = 0x014;
    /// Pause filter threshold (15:0).
    pub const PAUSE_FILTER_THRESHOLD: usize = 0x03C;
    /// Pause filter count (15:0).
    pub const PAUSE_FILTER_COUNT: usize = 0x03E;
    /// IOPM base physical address (bits 11:0 ignored).
    pub const IOPM_BASE: usize = 0x040;
    /// MSR permission map base physical address (bits 11:0 ignored).
    pub const MSRPM_BASE: usize = 0x048;
    /// TSC offset added to the guest's `RDTSC`.
    pub const TSC_OFFSET: usize = 0x050;
    /// Guest ASID (31:0), `TLB_CONTROL` (39:32), and the RAP bits 40–41.
    pub const GUEST_ASID: usize = 0x058;
    /// Virtual interrupt control (V_TPR, V_IRQ, ... bit 31 AVIC).
    pub const V_INTR: usize = 0x060;
    /// Interrupt shadow (bit 0) — set while `STI`/`MOV SS` blocking holds.
    pub const INT_SHADOW: usize = 0x068;
    /// Exit code of the last `#VMEXIT` (Appendix C).
    pub const EXITCODE: usize = 0x070;
    /// Exit info 1 (meaning depends on [`EXITCODE`]).
    pub const EXITINFO1: usize = 0x078;
    /// Exit info 2 (meaning depends on [`EXITCODE`]).
    pub const EXITINFO2: usize = 0x080;
    /// Exit interrupt info (for `INTR`/exception exits).
    pub const EXITINTINFO: usize = 0x088;
    /// Nested paging enable (bit 0), SEV bits, PML (bit 11).
    pub const NP_ENABLE: usize = 0x090;
    /// Virtual APIC page base (`AVIC`, bit 31 of [`V_INTR`]).
    pub const AVIC_APIC_BAR: usize = 0x098;
    /// SEV-ES GHCB page base.
    pub const GHCB: usize = 0x0A0;
    /// Event injection descriptor.
    pub const EVENTINJ: usize = 0x0A8;
    /// Guest nested page-table root.
    pub const N_CR3: usize = 0x0B0;
    /// VMCB clean bits (31:0); 0 = all dirty, which is all a seed needs.
    pub const VMCB_CLEAN: usize = 0x0C0;
    /// Next RIP, saved when the NRIPS feature is present (§15.28.x) for
    /// trap-style intercepts.
    pub const NRIP: usize = 0x0C8;
    /// Guest instruction bytes: bits 7:0 of the qword = byte count,
    /// bits 127:8 = the bytes, **last byte nearest the count**.
    pub const GUEST_INSTR: usize = 0x0D0;
    /// REQUESTED_IRR (255:0), for AVIC debugging.
    pub const REQUESTED_IRR: usize = 0x150;
    /// Exit interrupt data (`EXITINTINFO` payload).
    pub const EXITINTDATA: usize = 0x170;
    /// Event injection data payload.
    pub const EVENTINJDATA: usize = 0x178;
    /// PML page base (PML_ENABLE, [`NP_ENABLE`] bit 11).
    pub const PML_BASE: usize = 0x1C8;
    /// PML next index.
    pub const PML_INDEX: usize = 0x1D0;

    /// `TLB_CONTROL` encodings (39:32 of [`GUEST_ASID`]).
    pub const TLB_DO_NOTHING: u8 = 0;
    /// Flush the entire TLB on `VMRUN`.
    pub const TLB_FLUSH_ALL: u8 = 1;
    /// Flush this guest's TLB entries (by ASID).
    pub const TLB_FLUSH_GUEST: u8 = 3;
    /// Flush this guest's non-global entries.
    pub const TLB_FLUSH_GUEST_NONGLOBAL: u8 = 7;
}

/// Instruction intercept bits (Table B-1, `00Ch`/`010h`/`014h`).
pub mod intr0 {
    /// Physical interrupt.
    pub const INTR: u32 = 1 << 0;
    /// NMI.
    pub const NMI: u32 = 1 << 1;
    /// SMI.
    pub const SMI: u32 = 1 << 2;
    /// INIT.
    pub const INIT: u32 = 1 << 3;
    /// Virtual interrupt.
    pub const VINTR: u32 = 1 << 4;
    /// CR0 writes other than 0→TS/MP.
    pub const CR0_WR_OTHER: u32 = 1 << 5;
    /// IDTR read.
    pub const IDTR_READ: u32 = 1 << 6;
    /// GDTR read.
    pub const GDTR_READ: u32 = 1 << 7;
    /// LDTR read.
    pub const LDTR_READ: u32 = 1 << 8;
    /// TR read.
    pub const TR_READ: u32 = 1 << 9;
    /// IDTR write.
    pub const IDTR_WRITE: u32 = 1 << 10;
    /// GDTR write.
    pub const GDTR_WRITE: u32 = 1 << 11;
    /// LDTR write.
    pub const LDTR_WRITE: u32 = 1 << 12;
    /// TR write.
    pub const TR_WRITE: u32 = 1 << 13;
    /// `RDTSC`.
    pub const RDTSC: u32 = 1 << 14;
    /// `RDPMC`.
    pub const RDPMC: u32 = 1 << 15;
    /// `PUSHF`.
    pub const PUSHF: u32 = 1 << 16;
    /// `POPF`.
    pub const POPF: u32 = 1 << 17;
    /// `CPUID`.
    pub const CPUID: u32 = 1 << 18;
    /// `RSM`.
    pub const RSM: u32 = 1 << 19;
    /// `IRET`.
    pub const IRET: u32 = 1 << 20;
    /// `INTn` (software interrupt).
    pub const INTN: u32 = 1 << 21;
    /// `INVD`.
    pub const INVD: u32 = 1 << 22;
    /// `PAUSE`.
    pub const PAUSE: u32 = 1 << 23;
    /// `HLT`.
    pub const HLT: u32 = 1 << 24;
    /// `INVLPG`.
    pub const INVLPG: u32 = 1 << 25;
    /// `INVLPGA`.
    pub const INVLPGA: u32 = 1 << 26;
    /// I/O instruction protection (via the IOPM).
    pub const IOIO_PROT: u32 = 1 << 27;
    /// MSR protection (via the MSRPM).
    pub const MSR_PROT: u32 = 1 << 28;
    /// Task switch.
    pub const TASK_SWITCH: u32 = 1 << 29;
    /// FERR_FREEZE.
    pub const FERR_FREEZE: u32 = 1 << 30;
    /// Shutdown.
    pub const SHUTDOWN: u32 = 1 << 31;
}

/// Instruction intercept dword 1 bits (`010h`).
pub mod intr1 {
    /// **Must be set** — §15.5.1 makes a clear VMRUN-intercept bit
    /// `VMEXIT_INVALID`.
    pub const VMRUN: u32 = 1 << 0;
    /// `VMMCALL`.
    pub const VMMCALL: u32 = 1 << 1;
    /// `VMLOAD`.
    pub const VMLOAD: u32 = 1 << 2;
    /// `VMSAVE`.
    pub const VMSAVE: u32 = 1 << 3;
    /// `STGI`.
    pub const STGI: u32 = 1 << 4;
    /// `CLGI`.
    pub const CLGI: u32 = 1 << 5;
    /// `SKINIT`.
    pub const SKINIT: u32 = 1 << 6;
    /// `RDTSCP`.
    pub const RDTSCP: u32 = 1 << 7;
    /// `ICEBP`/`INT1`.
    pub const ICEBP: u32 = 1 << 8;
    /// `WBINVD`.
    pub const WBINVD: u32 = 1 << 9;
    /// `MONITOR`.
    pub const MONITOR: u32 = 1 << 10;
    /// `MWAIT` (armed).
    pub const MWAIT_ARMED: u32 = 1 << 11;
    /// `MWAIT` (not armed).
    pub const MWAIT_UNARMED: u32 = 1 << 12;
    /// `XSETBV`.
    pub const XSETBV: u32 = 1 << 13;
    /// `RDPRU`.
    pub const RDPRU: u32 = 1 << 14;
    /// EFER-write trap.
    pub const EFER_WRITE: u32 = 1 << 15;
    /// CR0-write trap.
    pub const CR0_WRITE: u32 = 1 << 16;
    /// CR3-write trap.
    pub const CR3_WRITE: u32 = 1 << 19;
    /// CR4-write trap.
    pub const CR4_WRITE: u32 = 1 << 20;
}

/// Instruction intercept dword 2 bits (`014h`) — newer TLB/busclock knobs,
/// listed for completeness; a seed sets none.
pub mod intr2 {
    /// `INVLPGB` (all).
    pub const INVLPGB_ALL: u32 = 1 << 0;
    /// `INVLPG` illegal form.
    pub const INVLPGB_ILLEGAL: u32 = 1 << 1;
    /// `INVPCID`.
    pub const INVPCID: u32 = 1 << 2;
    /// `MCOMMIT`.
    pub const MCOMMIT: u32 = 1 << 3;
    /// `TLBSYNC`.
    pub const TLBSYNC: u32 = 1 << 4;
    /// Bus lock threshold.
    pub const BUSLOCK_THRESHOLD: u32 = 1 << 5;
    /// HLT when no virtual interrupt is pending.
    pub const HLT_NO_VINTR: u32 = 1 << 6;
}

/// Exception intercept bits (byte-wide vectors in `008h`, as `u32`).
pub mod excp {
    /// #DB.
    pub const DB: u32 = 1 << 1;
    /// #UD.
    pub const UD: u32 = 1 << 6;
    /// #GP.
    pub const GP: u32 = 1 << 13;
    /// #PF.
    pub const PF: u32 = 1 << 14;
}

/// State save area offsets, **relative to `0400h`** in the VMCB page
/// (Table B-2).
pub mod save {
    /// ES descriptor, 16 B: selector @+0, attrib @+2, limit @+4, base @+8.
    pub const ES: usize = 0x000;
    /// CS descriptor (same 16 B layout).
    pub const CS: usize = 0x010;
    /// SS descriptor.
    pub const SS: usize = 0x020;
    /// DS descriptor — base is the **lower 32 bits only** in this table.
    pub const DS: usize = 0x030;
    /// FS descriptor.
    pub const FS: usize = 0x040;
    /// GS descriptor — full 64-bit base.
    pub const GS: usize = 0x050;
    /// GDTR (limit `+4` lower 16 bits, base `+8`).
    pub const GDTR: usize = 0x060;
    /// LDTR.
    pub const LDTR: usize = 0x070;
    /// IDTR (limit `+4`, base `+8`).
    pub const IDTR: usize = 0x080;
    /// TR (base `+8`).
    pub const TR: usize = 0x090;
    /// Guest CPL, one byte. **CPL comes from here, not from CS.DPL.**
    pub const CPL: usize = 0x0CB;
    /// Guest EFER.
    pub const EFER: usize = 0x0D0;
    /// Guest CR4.
    pub const CR4: usize = 0x148;
    /// Guest CR3.
    pub const CR3: usize = 0x150;
    /// Guest CR0.
    pub const CR0: usize = 0x158;
    /// Guest DR7.
    pub const DR7: usize = 0x160;
    /// Guest DR6.
    pub const DR6: usize = 0x168;
    /// Guest RFLAGS.
    pub const RFLAGS: usize = 0x170;
    /// Guest RIP.
    pub const RIP: usize = 0x178;
    /// Guest RSP.
    pub const RSP: usize = 0x1D8;
    /// Guest RAX.
    pub const RAX: usize = 0x1F8;
    /// Guest CR2.
    pub const CR2: usize = 0x240;
    /// Guest PAT.
    pub const G_PAT: usize = 0x268;
    /// `LASTEXCPFROM` (presumed `290h`; **not yet verified** — Table B-2
    /// transcription ends at `288h`).
    pub const LASTEXCPFROM: usize = 0x288;
}

/// Segment attribute word, 12 bits = descriptor bits 55:52 | 47:40
/// (Table B-2 note). A NULL segment is an all-zero attribute (`P = 0`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SegAttr(pub u16);

impl SegAttr {
    /// Descriptor type field (bits 3:0).
    #[must_use]
    pub const fn ty(self) -> u8 {
        (self.0 & 0xF) as u8
    }
    /// S bit (bit 4): 1 = code/data, 0 = system.
    #[must_use]
    pub const fn is_code_data(self) -> bool {
        self.0 & (1 << 4) != 0
    }
    /// DPL (bits 6:5). The hardware observes it for DS/ES/FS/GS; the **guest
    /// CPL is the separate byte at save-relative `0CBh`**.
    #[must_use]
    pub const fn dpl(self) -> u8 {
        ((self.0 >> 5) & 3) as u8
    }
    /// Present (bit 7).
    #[must_use]
    pub const fn is_present(self) -> bool {
        self.0 & (1 << 7) != 0
    }
    /// L bit (bit 9): 64-bit code segment.
    #[must_use]
    pub const fn is_long(self) -> bool {
        self.0 & (1 << 9) != 0
    }
    /// D/B bit (bit 10): 32-bit default size.
    #[must_use]
    pub const fn is_db(self) -> bool {
        self.0 & (1 << 10) != 0
    }
    /// Granularity (bit 11).
    #[must_use]
    pub const fn is_4k_granular(self) -> bool {
        self.0 & (1 << 11) != 0
    }
    /// Assemble from descriptor fields: `ty` (4 bits), `s`, `dpl`, `p`,
    /// `avl`, `l`, `db`, `g`.
    #[must_use]
    #[allow(clippy::many_single_char_names, clippy::fn_params_excessive_bools)]
    // The descriptor's own layout is eight one-letter fields (APM Vol 3
    // "Segment Descriptor"); the names mirror the spec, not Rust style.
    pub const fn from_fields(ty: u8, s: bool, dpl: u8, p: bool, avl: bool, l: bool, db: bool, g: bool) -> Self {
        let mut v = (ty & 0xF) as u16;
        if s {
            v |= 1 << 4;
        }
        v |= ((dpl & 3) as u16) << 5;
        if p {
            v |= 1 << 7;
        }
        if avl {
            v |= 1 << 8;
        }
        if l {
            v |= 1 << 9;
        }
        if db {
            v |= 1 << 10;
        }
        if g {
            v |= 1 << 11;
        }
        Self(v)
    }
    /// 32-bit flat code, DPL 0, accessed: type `B`, S=1, P=1, DB=1, G=1.
    /// G=1 pairs with limit `0xFFFFF` to make the limit 4 GiB — with G=0
    /// the raw 32-bit limit field tops out at 1 MiB, which is not flat.
    pub const CODE32_DPL0: Self = Self::from_fields(0xB, true, 0, true, false, false, true, true);
    /// 32-bit read/write data, DPL 0, accessed: type `3`, S=1, P=1, G=1
    /// (the rung-0 guest's SS/DS/ES; same G=1/limit `0xFFFFF` pairing).
    pub const DATA32_RW_DPL0: Self = Self::from_fields(0x3, true, 0, true, false, false, false, true);
    /// 64-bit code, DPL 0, accessed: type `B`, S=1, P=1, **L=1, D=0**.
    /// With paging on the two together are the long-mode state §15.5.1
    /// demands (`LongModeLAndD` rejects L and D set at once); limit and G
    /// are ignored in 64-bit mode and left clear.
    pub const CODE64_DPL0: Self = Self::from_fields(0xB, true, 0, true, false, true, false, false);
}

/// One decoded segment register from the save area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seg {
    /// Selector as loaded (unmasked).
    pub selector: u16,
    /// Attribute word (see [`SegAttr`]; NULL segment = all zero).
    pub attr: SegAttr,
    /// Limit.
    pub limit: u32,
    /// Base (full 64 bits; note Table B-2 records DS's base as its lower
    /// 32 bits).
    pub base: u64,
}

/// `#VMEXIT` exit codes, Appendix C. Only the ones the seed can actually see
/// are named; anything else decodes to [`ExitCode::Other`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitCode {
    /// `#DB` (vector 1) — exception exits are `040h + vector`.
    Db,
    /// `#UD` (vector 6).
    Ud,
    /// `#GP` (vector 13).
    Gp,
    /// `#PF` (vector 14).
    Pf,
    /// VINTR.
    Vintr,
    /// `CPUID`.
    Cpuid,
    /// `PAUSE` — hardware **advances RIP** on this exit.
    Pause,
    /// `HLT` — hardware **advances RIP** on this exit.
    Hlt,
    /// I/O instruction.
    Ioio,
    /// MSR.
    Msr,
    /// `VMRUN`.
    Vmrun,
    /// `VMMCALL`.
    Vmmcall,
    /// `RDTSCP`.
    Rdtscp,
    /// Nested page fault.
    Npf,
    /// `VMEXIT_INVALID`: a VMRUN consistency check failed (§15.5.1). The
    /// checks that produced it are in [`validate_guest_state`].
    Invalid,
    /// `VMEXIT_BUSY` (unlaunched VMCB in use).
    Busy,
    /// Anything else — value as in Appendix C.
    Other(u64),
}

impl ExitCode {
    /// Decode Appendix C.
    #[must_use]
    pub const fn from_u64(v: u64) -> Self {
        match v {
            0x40 | 0x41 => Self::Db,
            0x46 => Self::Ud,
            0x4D => Self::Gp,
            0x4E => Self::Pf,
            0x64 => Self::Vintr,
            0x72 => Self::Cpuid,
            0x77 => Self::Pause,
            0x78 => Self::Hlt,
            0x7B => Self::Ioio,
            0x7C => Self::Msr,
            0x80 => Self::Vmrun,
            0x81 => Self::Vmmcall,
            0x87 => Self::Rdtscp,
            0x400 => Self::Npf,
            0xFFFF_FFFF => Self::Invalid,
            0xFFFF_FFFE => Self::Busy,
            other => Self::Other(other),
        }
    }
}

/// A VMCB page, typed.
///
/// Construct over an **already zero-filled** page; see the crate header for
/// why. All accessors are bounds-checked indexing into the page (which is
/// exactly `[u8; 4096]`, so the const offsets compile to addresses and
/// cannot fail at runtime).
#[derive(Debug)]
pub struct Vmcb<'a> {
    page: &'a mut [u8; VMCB_SIZE],
}

#[must_use]
pub const fn rd16(page: &[u8; VMCB_SIZE], off: usize) -> u16 {
    u16::from_le_bytes([page[off], page[off + 1]])
}

#[must_use]
pub const fn rd32(page: &[u8; VMCB_SIZE], off: usize) -> u32 {
    u32::from_le_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]])
}

#[must_use]
pub const fn rd64(page: &[u8; VMCB_SIZE], off: usize) -> u64 {
    u64::from_le_bytes([
        page[off],
        page[off + 1],
        page[off + 2],
        page[off + 3],
        page[off + 4],
        page[off + 5],
        page[off + 6],
        page[off + 7],
    ])
}

pub fn wr16(page: &mut [u8; VMCB_SIZE], off: usize, v: u16) {
    page[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn wr32(page: &mut [u8; VMCB_SIZE], off: usize, v: u32) {
    page[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn wr64(page: &mut [u8; VMCB_SIZE], off: usize, v: u64) {
    page[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

impl<'a> Vmcb<'a> {
    /// Type a (zero-filled) page as a VMCB.
    pub const fn new(page: &'a mut [u8; VMCB_SIZE]) -> Self {
        Self { page }
    }

    /// The raw page (for logging a hexdump of the exit state).
    #[must_use]
    pub const fn page(&self) -> &[u8; VMCB_SIZE] {
        self.page
    }

    // ---- control area ----

    /// Set instruction intercept dword 0 (`00Ch`) wholesale.
    pub fn set_intr0(&mut self, bits: u32) {
        wr32(self.page, ctrl::INSTR_INTERCEPT0, bits);
    }
    /// Set instruction intercept dword 1 (`010h`) wholesale.
    pub fn set_intr1(&mut self, bits: u32) {
        wr32(self.page, ctrl::INSTR_INTERCEPT1, bits);
    }
    /// Set instruction intercept dword 2 (`014h`) wholesale.
    pub fn set_intr2(&mut self, bits: u32) {
        wr32(self.page, ctrl::INSTR_INTERCEPT2, bits);
    }
    /// Set the exception intercept bitmap (`008h`): bit = vector.
    pub fn set_excp(&mut self, bits: u32) {
        wr32(self.page, ctrl::EXCP_INTERCEPT, bits);
    }
    /// Exception intercept bitmap, for post-mortem.
    #[must_use]
    pub const fn excp(&self) -> u32 {
        rd32(self.page, ctrl::EXCP_INTERCEPT)
    }
    /// Guest ASID. **0 is illegal** (§15.5.1) and is rejected by
    /// [`validate_guest_state`].
    pub fn set_asid(&mut self, asid: u32) {
        wr32(self.page, ctrl::GUEST_ASID, asid);
    }
    /// Guest ASID.
    #[must_use]
    pub const fn asid(&self) -> u32 {
        rd32(self.page, ctrl::GUEST_ASID)
    }
    /// `TLB_CONTROL` (39:32 of `058h`).
    pub fn set_tlb_control(&mut self, v: u8) {
        self.page[ctrl::GUEST_ASID + 4] = v;
    }
    /// Nested paging enable (`090h` bit 0).
    pub fn set_np_enable(&mut self, on: bool) {
        let b = rd32(self.page, ctrl::NP_ENABLE);
        wr32(self.page, ctrl::NP_ENABLE, if on { b | 1 } else { b & !1 });
    }
    /// Nested paging enable.
    #[must_use]
    pub const fn np_enable(&self) -> bool {
        rd32(self.page, ctrl::NP_ENABLE) & 1 != 0
    }
    /// Guest nested page-table root (`0B0h`).
    pub fn set_n_cr3(&mut self, pa: u64) {
        wr64(self.page, ctrl::N_CR3, pa);
    }
    /// Guest nested page-table root.
    #[must_use]
    pub const fn n_cr3(&self) -> u64 {
        rd64(self.page, ctrl::N_CR3)
    }
    /// IOPM base (bits 11:0 are ignored by the hardware; caller aligns).
    pub fn set_iopm_base(&mut self, pa: u64) {
        wr64(self.page, ctrl::IOPM_BASE, pa);
    }
    /// MSRPM base (bits 11:0 ignored).
    pub fn set_msrpm_base(&mut self, pa: u64) {
        wr64(self.page, ctrl::MSRPM_BASE, pa);
    }
    /// Exit code of the last `#VMEXIT`.
    #[must_use]
    pub const fn exitcode(&self) -> u64 {
        rd64(self.page, ctrl::EXITCODE)
    }
    /// Exit info 1.
    #[must_use]
    pub const fn exitinfo1(&self) -> u64 {
        rd64(self.page, ctrl::EXITINFO1)
    }
    /// Exit info 2.
    #[must_use]
    pub const fn exitinfo2(&self) -> u64 {
        rd64(self.page, ctrl::EXITINFO2)
    }
    /// Exit interrupt info.
    #[must_use]
    pub const fn exitintinfo(&self) -> u64 {
        rd64(self.page, ctrl::EXITINTINFO)
    }
    /// Next RIP (`0C8h`), saved with NRIPS; 0 before the first run.
    #[must_use]
    pub const fn nrip(&self) -> u64 {
        rd64(self.page, ctrl::NRIP)
    }
    /// Guest instruction bytes captured at the exit: `(count, bytes)` with
    /// the **last** instruction byte at `bytes[0]` (bits 127:8 run backwards
    /// from the count field).
    #[must_use]
    pub const fn guest_instr(&self) -> (usize, [u8; 15]) {
        let cnt = (rd64(self.page, ctrl::GUEST_INSTR) & 0xFF) as usize;
        let mut bytes = [0u8; 15];
        let mut i = 0;
        while i < 15 {
            bytes[i] = self.page[ctrl::GUEST_INSTR + 1 + i];
            i += 1;
        }
        (cnt, bytes)
    }

    // ---- state save area ----

    /// Read a segment descriptor.
    #[must_use]
    pub const fn seg(&self, off: usize) -> Seg {
        let base = SAVE_AREA_BASE + off;
        Seg {
            selector: rd16(self.page, base),
            attr: SegAttr(rd16(self.page, base + 2)),
            limit: rd32(self.page, base + 4),
            base: rd64(self.page, base + 8),
        }
    }
    /// Write a segment descriptor.
    pub fn set_seg(&mut self, off: usize, seg: Seg) {
        let base = SAVE_AREA_BASE + off;
        wr16(self.page, base, seg.selector);
        wr16(self.page, base + 2, seg.attr.0);
        wr32(self.page, base + 4, seg.limit);
        wr64(self.page, base + 8, seg.base);
    }
    /// Guest CPL (save-relative `0CBh` — not CS.DPL).
    pub const fn set_cpl(&mut self, cpl: u8) {
        self.page[SAVE_AREA_BASE + save::CPL] = cpl;
    }
    /// Guest CPL.
    #[must_use]
    pub const fn cpl(&self) -> u8 {
        self.page[SAVE_AREA_BASE + save::CPL]
    }
    /// Guest EFER.
    pub fn set_efer(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::EFER, v);
    }
    /// Guest EFER.
    #[must_use]
    pub const fn efer(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::EFER)
    }
    /// Guest CR4.
    pub fn set_cr4(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::CR4, v);
    }
    /// Guest CR4.
    #[must_use]
    pub const fn cr4(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::CR4)
    }
    /// Guest CR3.
    pub fn set_cr3(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::CR3, v);
    }
    /// Guest CR3.
    #[must_use]
    pub const fn cr3(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::CR3)
    }
    /// Guest CR0.
    pub fn set_cr0(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::CR0, v);
    }
    /// Guest CR0.
    #[must_use]
    pub const fn cr0(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::CR0)
    }
    /// Guest DR6.
    #[must_use]
    pub const fn dr6(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::DR6)
    }
    /// Guest DR7.
    #[must_use]
    pub const fn dr7(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::DR7)
    }
    /// Guest RFLAGS.
    pub fn set_rflags(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RFLAGS, v);
    }
    /// Guest RFLAGS.
    #[must_use]
    pub const fn rflags(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RFLAGS)
    }
    /// Guest RIP.
    pub fn set_rip(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RIP, v);
    }
    /// Guest RIP.
    #[must_use]
    pub const fn rip(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RIP)
    }
    /// Guest RSP.
    pub fn set_rsp(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RSP, v);
    }
    /// Guest RSP.
    #[must_use]
    pub const fn rsp(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RSP)
    }
    /// Guest RAX.
    pub fn set_rax(&mut self, v: u64) {
        wr64(self.page, SAVE_AREA_BASE + save::RAX, v);
    }
    /// Guest RAX.
    #[must_use]
    pub const fn rax(&self) -> u64 {
        rd64(self.page, SAVE_AREA_BASE + save::RAX)
    }
}

/// CR0 bits the consistency checks name (APM Vol 2 §15.5.1; encodings Vol 3).
pub mod cr0 {
    /// PE — protection enable.
    pub const PE: u64 = 1 << 0;
    /// NE — numeric error.
    pub const NE: u64 = 1 << 5;
    /// NW — write through.
    pub const NW: u64 = 1 << 29;
    /// CD — cache disable.
    pub const CD: u64 = 1 << 30;
    /// PG — paging enable.
    pub const PG: u64 = 1 << 31;
}

/// CR4 bits the consistency checks name.
pub mod cr4 {
    /// PAE — physical address extension.
    pub const PAE: u64 = 1 << 6;
}

/// A VMRUN consistency check that the guest state violates (§15.5.1).
///
/// The hardware reports all of these the same way — `VMEXIT_INVALID`, no
/// diagnostics — so the checks are reimplemented here, where a failing boot
/// names its own cause.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Illicit {
    /// Guest `EFER.SVME` must be 1.
    SvmeClear,
    /// `CR0.CD = 0` with `CR0.NW = 1`.
    Cr0CdWithoutNw,
    /// `CR0[63:32] != 0`.
    Cr0HighBits,
    /// `DR6[63:32] != 0`.
    Dr6HighBits,
    /// `DR7[63:32] != 0`.
    Dr7HighBits,
    /// `EFER.LMA` set without `EFER.LME`.
    LmaWithoutLme,
    /// `LME & PG` without `CR4.PAE`.
    LmePgWithoutPae,
    /// `LME & PG` without `CR0.PE`.
    LmePgWithoutPe,
    /// `LME & PG & PAE` with both `CS.L` and `CS.D` set.
    LongModeLAndD,
    /// Guest ASID is 0.
    AsidZero,
    /// The VMRUN intercept bit is not set — §15.5.1 requires the VMCB to
    /// intercept VMRUN itself.
    VmrunNotIntercepted,
}

/// Check a VMCB's guest state against the §15.5.1 list. `Ok` means VMRUN
/// should not fail with `VMEXIT_INVALID` *for the checks implemented here*.
///
/// Deliberately **not** implemented, pending exact bit tables from the spec
/// transcription: the MBZ-bit sets of CR3/CR4 and the EFER reserved bits
/// (Vol 3 owns the encodings; transcribe before the Linux rung), MSRPM/IOPM
/// beyond max physical address (needs the box's max PA), event-injection
/// legality (the seed never injects: EVENTINJ stays 0), and the S_CET checks
/// (CET unused).
pub fn validate_guest_state(vmcb: &Vmcb<'_>) -> Result<(), Illicit> {
    use cr0::{CD, NW, PE, PG};
    use msr::{EFER_LMA, EFER_LME, EFER_SVME};

    let efer = vmcb.efer();
    let cr0v = vmcb.cr0();
    let cr4v = vmcb.cr4();

    if efer & EFER_SVME == 0 {
        return Err(Illicit::SvmeClear);
    }
    if cr0v & CD == 0 && cr0v & NW != 0 {
        return Err(Illicit::Cr0CdWithoutNw);
    }
    if cr0v >> 32 != 0 {
        return Err(Illicit::Cr0HighBits);
    }
    if vmcb.dr6() >> 32 != 0 {
        return Err(Illicit::Dr6HighBits);
    }
    if vmcb.dr7() >> 32 != 0 {
        return Err(Illicit::Dr7HighBits);
    }
    if efer & EFER_LMA != 0 && efer & EFER_LME == 0 {
        return Err(Illicit::LmaWithoutLme);
    }
    if efer & EFER_LME != 0 && cr0v & PG != 0 && cr4v & cr4::PAE == 0 {
        return Err(Illicit::LmePgWithoutPae);
    }
    if efer & EFER_LME != 0 && cr0v & PG != 0 && cr0v & PE == 0 {
        return Err(Illicit::LmePgWithoutPe);
    }
    if efer & EFER_LME != 0 && cr0v & PG != 0 && cr4v & cr4::PAE != 0 {
        let cs = vmcb.seg(save::CS);
        if cs.attr.is_long() && cs.attr.is_db() {
            return Err(Illicit::LongModeLAndD);
        }
    }
    if vmcb.asid() == 0 {
        return Err(Illicit::AsidZero);
    }
    if !vmcb.page().is_empty() && rd32(vmcb.page(), ctrl::INSTR_INTERCEPT1) & intr1::VMRUN == 0 {
        return Err(Illicit::VmrunNotIntercepted);
    }
    Ok(())
}

impl Illicit {
    /// The static name, for a serial line that cannot allocate.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SvmeClear => "SvmeClear",
            Self::Cr0CdWithoutNw => "Cr0CdWithoutNw",
            Self::Cr0HighBits => "Cr0HighBits",
            Self::Dr6HighBits => "Dr6HighBits",
            Self::Dr7HighBits => "Dr7HighBits",
            Self::LmaWithoutLme => "LmaWithoutLme",
            Self::LmePgWithoutPae => "LmePgWithoutPae",
            Self::LmePgWithoutPe => "LmePgWithoutPe",
            Self::LongModeLAndD => "LongModeLAndD",
            Self::AsidZero => "AsidZero",
            Self::VmrunNotIntercepted => "VmrunNotIntercepted",
        }
    }
}

/// Nested page-table entry encodings.
///
/// **Subset pending verification:** the exact NPT leaf bit table (A/D
/// semantics, PKS) is not yet transcribed from Vol 2 §"Nested Table Walk";
/// what §15.5.x and the NP text pin is Present / R / W / `PS` / the physical
/// address field, and that A/D are hardware-set. Re-verify before the Linux
/// rung before adding bits.
pub mod npt {
    /// Present.
    pub const P: u64 = 1 << 0;
    /// Read/write.
    pub const RW: u64 = 1 << 1;
    /// User.
    pub const US: u64 = 1 << 2;
    /// Page size — a 2 MiB leaf at the PD level.
    pub const PS: u64 = 1 << 7;
    /// Physical address field of an NPT entry: bits 51:12.
    pub const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

    /// An interior (pointer) entry pointing at `child_pa`. Guest accesses walk
    /// the NPT as user-mode accesses (APM Vol 2 §15.25.5), so every entry needs
    /// U=1 or a present page still faults (NPF, P=1).
    #[must_use]
    pub const fn pointer(child_pa: u64) -> u64 {
        (child_pa & ADDR_MASK) | P | RW | US
    }
    /// A 2 MiB leaf mapping `pa` (which must be 2 MiB aligned).
    #[must_use]
    pub const fn leaf_2m(pa: u64) -> u64 {
        (pa & ADDR_MASK) | P | RW | US | PS
    }
}

/// The MSR permissions map (§15.11).
///
/// Four 16-Kbit vectors packed into **two contiguous physical pages**
/// (8 KiB; `MSRPM_BASE_PA` is 4-KiB aligned, bits 11:0 ignored, and a map
/// reaching past maximum physical address is `VMEXIT_INVALID`). Two bits
/// per MSR, **lsb read, msb write, 1 = intercept**; a MSR the map does not
/// cover intercepts unconditionally when `intr0::MSR_PROT` is set. The
/// vectors (Table 158):
///
/// | byte offsets     | MSRs covered            |
/// |------------------|-------------------------|
/// | `0000h`–`07FFh`  | `0000_0000h–0000_1FFFh` |
/// | `0800h`–`0FFFh`  | `C000_0000h–C000_1FFFh` |
/// | `1000h`–`17FFh`  | `C001_0000h–C001_1FFFh` |
/// | `1800h`–`1FFFh`  | reserved                |
pub mod msrpm {
    /// The whole map: two contiguous pages.
    pub const SIZE: usize = 8192;
    /// One 16-Kbit vector: 8192 MSRs × 2 bits.
    pub const VECTOR: usize = 2048;

    /// Byte index and bit position of the two bits covering `msr`, or
    /// `None` for an MSR in a reserved vector (auto-intercept).
    #[must_use]
    pub const fn bits_of(msr: u32) -> Option<(usize, u8)> {
        let area = match msr {
            0x0000_0000..=0x0000_1FFF => 0,
            0xC000_0000..=0xC000_1FFF => 1,
            0xC001_0000..=0xC001_1FFF => 2,
            _ => return None,
        };
        let off = (msr & 0x1FFF) as usize;
        Some((area * VECTOR + off / 4, ((off % 4) * 2) as u8))
    }

    /// `page` with the two bits covering `msr` set to intercept reads
    /// and/or writes. Const so a fixed policy map can arm a `static`.
    #[must_use]
    pub const fn with_access(page: [u8; SIZE], msr: u32, read: bool, write: bool) -> [u8; SIZE] {
        let mut page = page;
        if let Some((byte, bit)) = bits_of(msr) {
            page[byte] = if read {
                page[byte] | (1 << bit)
            } else {
                page[byte] & !(1 << bit)
            };
            page[byte] = if write {
                page[byte] | (1 << (bit + 1))
            } else {
                page[byte] & !(1 << (bit + 1))
            };
        }
        page
    }

    /// The two bits covering `msr` as `(read_intercepted,
    /// write_intercepted)`. A reserved-vector MSR reads as `(true, true)` —
    /// the auto-intercept the hardware applies.
    #[must_use]
    pub const fn bits(page: &[u8; SIZE], msr: u32) -> (bool, bool) {
        if let Some((byte, bit)) = bits_of(msr) {
            let v = page[byte] >> bit;
            (v & 1 != 0, v & 2 != 0)
        } else {
            (true, true)
        }
    }
}

/// The I/O permissions map (§15.10.1).
///
/// "A linear array of 64K+3 bits (two 4-Kbyte pages, and the first three
/// bits of a third 4-Kbyte page)" — **12 contiguous KiB**, 4-KiB aligned
/// (`IOPM_BASE_PA` bits 11:0 ignored; past maximum physical address is
/// `VMEXIT_INVALID`). One bit per 8-bit port, **1 = intercept**, bit 0 =
/// port 0.
pub mod iopm {
    /// The whole map: three contiguous pages (§15.10.1: "12 Kbytes").
    pub const SIZE: usize = 12288;
    /// The ports the map names; the "+3 bits" name ports that do not exist.
    pub const PORTS: usize = 65536;

    /// `page` with the bit for `port` set to intercept or allow. Const so a
    /// fixed policy map can arm a `static`.
    #[must_use]
    pub const fn with_port(page: [u8; SIZE], port: u16, on: bool) -> [u8; SIZE] {
        let bit = port as usize;
        let mask = 1u8 << (bit % 8);
        let mut page = page;
        page[bit / 8] = if on { page[bit / 8] | mask } else { page[bit / 8] & !mask };
        page
    }

    /// Is access to `port` intercepted?
    #[must_use]
    pub const fn intercepted(page: &[u8; SIZE], port: u16) -> bool {
        let bit = port as usize;
        page[bit / 8] & (1 << (bit % 8)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VMCB page with one recognizable byte pattern everywhere, so an
    /// accessor that reads the wrong offset fails loudly rather than
    /// reading another field's zero.
    fn pattern_page() -> [u8; VMCB_SIZE] {
        let mut page = [0u8; VMCB_SIZE];
        for (i, b) in page.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        page
    }

    // ---- absolute offset pins: every Table B-1/B-2 fact the seed relies on ----

    #[test]
    fn control_offsets_are_table_b1() {
        let page = pattern_page();
        assert_eq!(ctrl::EXITCODE, 0x070);
        assert_eq!(rd64(&page, ctrl::EXITCODE), u64::from_le_bytes(page[0x070..0x078].try_into().unwrap()));
        assert_eq!(ctrl::EXITINFO1, 0x078);
        assert_eq!(ctrl::EXITINFO2, 0x080);
        assert_eq!(ctrl::EXITINTINFO, 0x088);
        assert_eq!(ctrl::N_CR3, 0x0B0);
        assert_eq!(ctrl::NRIP, 0x0C8);
        assert_eq!(ctrl::GUEST_INSTR, 0x0D0);
        assert_eq!(ctrl::GUEST_ASID, 0x058);
        assert_eq!(ctrl::NP_ENABLE, 0x090);
        assert_eq!(ctrl::IOPM_BASE, 0x040);
        assert_eq!(ctrl::MSRPM_BASE, 0x048);
    }

    #[test]
    fn save_offsets_are_table_b2() {
        // Save-area-relative offsets from the table; absolute in the page.
        for (rel, abs) in [
            (save::ES, 0x400),
            (save::CS, 0x410),
            (save::SS, 0x420),
            (save::DS, 0x430),
            (save::FS, 0x440),
            (save::GS, 0x450),
            (save::GDTR, 0x460),
            (save::IDTR, 0x480),
            (save::TR, 0x490),
            (save::CPL, 0x4CB),
            (save::EFER, 0x4D0),
            (save::CR4, 0x548),
            (save::CR3, 0x550),
            (save::CR0, 0x558),
            (save::DR7, 0x560),
            (save::DR6, 0x568),
            (save::RFLAGS, 0x570),
            (save::RIP, 0x578),
            (save::RSP, 0x5D8),
            (save::RAX, 0x5F8),
            (save::CR2, 0x640),
            (save::G_PAT, 0x668),
        ] {
            assert_eq!(SAVE_AREA_BASE + rel, abs, "offset 0x{rel:03X} must land at 0x{abs:03X}");
        }
    }

    #[test]
    fn typed_accessors_read_the_pinned_offsets() {
        // All raw writes happen before the VMCB is typed: one borrow for the
        // setup, one for the reads.
        let mut page = [0u8; VMCB_SIZE];
        wr64(&mut page, 0x4D0, 0x1234);
        wr64(&mut page, 0x578, 0xCAFE);
        page[0x4CB] = 3;
        wr32(&mut page, 0x058, 7);
        wr32(&mut page, 0x090, 1);
        wr64(&mut page, 0x0B0, 0xABCD_000);
        wr64(&mut page, 0x0C8, 0x5555);
        wr64(&mut page, 0x070, 0x81);
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(vmcb.efer(), 0x1234);
        assert_eq!(vmcb.rip(), 0xCAFE);
        assert_eq!(vmcb.cpl(), 3);
        assert_eq!(vmcb.asid(), 7);
        assert!(vmcb.np_enable());
        assert_eq!(vmcb.n_cr3(), 0xABCD_000);
        assert_eq!(vmcb.nrip(), 0x5555);
        assert_eq!(vmcb.exitcode(), 0x81);
        assert_eq!(rd64(vmcb.page(), ctrl::EXITCODE), vmcb.exitcode());
    }

    #[test]
    fn setters_write_the_pinned_offsets() {
        let mut page = [0u8; VMCB_SIZE];
        {
            let mut vmcb = Vmcb::new(&mut page);
            vmcb.set_efer(0x1111);
            vmcb.set_rip(0x2222);
            vmcb.set_rax(0x3333);
            vmcb.set_rsp(0x4444);
            vmcb.set_cr0(0x5555);
            vmcb.set_cr3(0x6666);
            vmcb.set_cr4(0x7777);
            vmcb.set_rflags(2);
            vmcb.set_cpl(0);
            vmcb.set_asid(1);
            vmcb.set_tlb_control(ctrl::TLB_DO_NOTHING);
            vmcb.set_n_cr3(0x88_000);
            vmcb.set_np_enable(true);
            vmcb.set_intr0(intr0::CPUID | intr0::HLT | intr0::IOIO_PROT | intr0::MSR_PROT | intr0::INTR);
            vmcb.set_intr1(intr1::VMRUN | intr1::VMMCALL);
            vmcb.set_excp(excp::DB | excp::UD | excp::GP | excp::PF);
        }
        assert_eq!(rd64(&page, 0x4D0), 0x1111);
        assert_eq!(rd64(&page, 0x578), 0x2222);
        assert_eq!(rd64(&page, 0x5F8), 0x3333);
        assert_eq!(rd64(&page, 0x5D8), 0x4444);
        assert_eq!(rd64(&page, 0x558), 0x5555);
        assert_eq!(rd64(&page, 0x550), 0x6666);
        assert_eq!(rd64(&page, 0x548), 0x7777);
        assert_eq!(rd64(&page, 0x570), 2);
        assert_eq!(page[0x4CB], 0);
        assert_eq!(rd32(&page, 0x058), 1);
        // TLB_CONTROL is 39:32 of 058h = byte 3 of the dword.
        assert_eq!(page[0x05B], 0);
        assert_eq!(rd32(&page, 0x090) & 1, 1);
        assert_eq!(rd32(&page, 0x00C), 1 | 1 << 18 | 1 << 24 | 1 << 27 | 1 << 28);
        assert_eq!(rd32(&page, 0x010), 1 | 1 << 1);
        assert_eq!(rd32(&page, 0x008), 1 << 1 | 1 << 6 | 1 << 13 | 1 << 14);
    }

    #[test]
    fn segment_roundtrip_matches_the_16_byte_layout() {
        let mut page = [0u8; VMCB_SIZE];
        let cs = Seg {
            selector: 0x08,
            attr: SegAttr::CODE32_DPL0,
            limit: 0xFFFF_FFFF,
            base: 0,
        };
        {
            let mut vmcb = Vmcb::new(&mut page);
            vmcb.set_seg(save::CS, cs);
        }
        // selector @+0, attrib @+2, limit @+4, base @+8 — Table B-2 note.
        assert_eq!(rd16(&page, 0x410), 0x08);
        assert_eq!(rd16(&page, 0x412), SegAttr::CODE32_DPL0.0);
        assert_eq!(rd32(&page, 0x414), 0xFFFF_FFFF);
        assert_eq!(rd64(&page, 0x418), 0);
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(vmcb.seg(save::CS), cs);
        assert_eq!(vmcb.seg(save::ES), Seg { selector: 0, attr: SegAttr(0), limit: 0, base: 0 });
    }

    #[test]
    fn segattr_decodes_the_documented_words() {
        // 0x49B = type B (code, exec/read, accessed), S=1, DPL=0, P=1,
        // L=0, DB=1, G=0 — the APM's worked example, before granularity.
        let cs = SegAttr(0x49B);
        assert_eq!(cs.ty(), 0xB);
        assert!(cs.is_code_data());
        assert_eq!(cs.dpl(), 0);
        assert!(cs.is_present());
        assert!(!cs.is_long());
        assert!(cs.is_db());
        assert!(!cs.is_4k_granular());
        // The preset turns granularity on (0x800) — flat 4 GiB needs it.
        assert_eq!(SegAttr::CODE32_DPL0.0, 0x49B | 0x800);
        // 0x93 = type 3 (data, read/write, accessed), S=1, DPL=0, P=1.
        let ds = SegAttr(0x93);
        assert_eq!(ds.ty(), 0x3);
        assert!(ds.is_code_data());
        assert_eq!(ds.dpl(), 0);
        assert!(ds.is_present());
        assert!(!ds.is_db());
        // The preset turns granularity on (0x800) — flat 4 GiB needs it.
        assert_eq!(SegAttr::DATA32_RW_DPL0.0, 0x93 | 0x800);
        // NULL segment: all zero, P clear.
        assert!(!SegAttr(0).is_present());
        // DPL occupies bits 6:5.
        assert_eq!(SegAttr::from_fields(0xB, true, 3, true, false, false, true, false).0, 0x4FB);
    }

    #[test]
    fn exitcodes_decode_appendix_c() {
        assert_eq!(ExitCode::from_u64(0x81), ExitCode::Vmmcall);
        assert_eq!(ExitCode::from_u64(0x78), ExitCode::Hlt);
        assert_eq!(ExitCode::from_u64(0x77), ExitCode::Pause);
        assert_eq!(ExitCode::from_u64(0x72), ExitCode::Cpuid);
        assert_eq!(ExitCode::from_u64(0x80), ExitCode::Vmrun);
        assert_eq!(ExitCode::from_u64(0x87), ExitCode::Rdtscp);
        assert_eq!(ExitCode::from_u64(0x7B), ExitCode::Ioio);
        assert_eq!(ExitCode::from_u64(0x7C), ExitCode::Msr);
        assert_eq!(ExitCode::from_u64(0x64), ExitCode::Vintr);
        assert_eq!(ExitCode::from_u64(0x400), ExitCode::Npf);
        assert_eq!(ExitCode::from_u64(0xFFFF_FFFF), ExitCode::Invalid);
        assert_eq!(ExitCode::from_u64(0xFFFF_FFFE), ExitCode::Busy);
        // Exception exits are 040h + vector (Appendix C).
        assert_eq!(ExitCode::from_u64(0x41), ExitCode::Db);
        assert_eq!(ExitCode::from_u64(0x46), ExitCode::Ud);
        assert_eq!(ExitCode::from_u64(0x4D), ExitCode::Gp);
        assert_eq!(ExitCode::from_u64(0x4E), ExitCode::Pf);
        assert_eq!(ExitCode::from_u64(0x123), ExitCode::Other(0x123));
    }

    #[test]
    fn intercept_bits_are_table_b1() {
        assert_eq!(intr0::CPUID, 1 << 18);
        assert_eq!(intr0::HLT, 1 << 24);
        assert_eq!(intr0::IOIO_PROT, 1 << 27);
        assert_eq!(intr0::MSR_PROT, 1 << 28);
        assert_eq!(intr0::INTR, 1 << 0);
        assert_eq!(intr1::VMRUN, 1 << 0);
        assert_eq!(intr1::VMMCALL, 1 << 1);
        assert_eq!(intr1::EFER_WRITE, 1 << 15);
        assert_eq!(excp::DB, 1 << 1);
        assert_eq!(excp::UD, 1 << 6);
        assert_eq!(excp::GP, 1 << 13);
        assert_eq!(excp::PF, 1 << 14);
    }

    #[test]
    fn guest_instr_bytes_run_backwards_from_the_count() {
        let mut page = [0u8; VMCB_SIZE];
        // An instruction's last byte lands nearest the count field:
        // bytes stored at +1.. are reversed relative to execution order.
        page[ctrl::GUEST_INSTR] = 3;
        page[ctrl::GUEST_INSTR + 1] = 0x0F; // last byte of the instruction
        page[ctrl::GUEST_INSTR + 2] = 0x01;
        page[ctrl::GUEST_INSTR + 3] = 0xD9; // first byte executed
        let vmcb = Vmcb::new(&mut page);
        let (cnt, bytes) = vmcb.guest_instr();
        assert_eq!(cnt, 3);
        assert_eq!(bytes[0], 0x0F);
        assert_eq!(bytes[1], 0x01);
        assert_eq!(bytes[2], 0xD9);
    }

    // ---- the §15.5.1 validator ----

    /// A guest state that must pass: the rung-0 shape — 32-bit flat
    /// protected mode, no guest paging, SVME on, ASID 1, VMRUN intercepted.
    fn rung0_page() -> [u8; VMCB_SIZE] {
        let mut page = [0u8; VMCB_SIZE];
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME);
        vmcb.set_cr0(cr0::PE | cr0::NE);
        vmcb.set_cr4(0);
        vmcb.set_rflags(2);
        vmcb.set_cpl(0);
        vmcb.set_asid(1);
        vmcb.set_intr1(intr1::VMRUN);
        let cs = Seg { selector: 0x08, attr: SegAttr::CODE32_DPL0, limit: 0xFFFF_FFFF, base: 0 };
        vmcb.set_seg(save::CS, cs);
        page
    }

    #[test]
    fn flat_protected_no_paging_guest_is_legal() {
        let mut page = rung0_page();
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(validate_guest_state(&vmcb), Ok(()));
    }

    #[test]
    fn each_consistency_check_fires_on_its_own_violation() {
        // SVME clear -> SvmeClear.
        let mut page = rung0_page();
        Vmcb::new(&mut page).set_efer(0);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::SvmeClear));

        // CD=0 with NW=1 -> Cr0CdWithoutNw.
        let mut page = rung0_page();
        Vmcb::new(&mut page).set_cr0(cr0::PE | cr0::NE | cr0::NW);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Cr0CdWithoutNw));
        // ...but CD=1 with NW=1 is the historical reset shape and is legal.
        let mut page = rung0_page();
        Vmcb::new(&mut page).set_cr0(cr0::PE | cr0::NE | cr0::NW | cr0::CD);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Ok(()));

        // CR0[63:32] set -> Cr0HighBits.
        let mut page = rung0_page();
        Vmcb::new(&mut page).set_cr0(cr0::PE | cr0::NE | 1 << 32);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Cr0HighBits));

        // DR6[63:32] set -> Dr6HighBits.
        let mut page = rung0_page();
        wr64(&mut page, 0x568, 1 << 32);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Dr6HighBits));

        // DR7[63:32] set -> Dr7HighBits.
        let mut page = rung0_page();
        wr64(&mut page, 0x560, 1 << 32);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::Dr7HighBits));

        // LMA without LME -> LmaWithoutLme.
        let mut page = rung0_page();
        Vmcb::new(&mut page).set_efer(msr::EFER_SVME | msr::EFER_LMA);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmaWithoutLme));

        // LME & PG without PAE -> LmePgWithoutPae.
        let mut page = rung0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME);
        vmcb.set_cr0(cr0::PE | cr0::NE | cr0::PG);
        drop(vmcb);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPae));

        // LME & PG without PE -> LmePgWithoutPe.
        let mut page = rung0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME);
        vmcb.set_cr0(cr0::NE | cr0::PG);
        vmcb.set_cr4(cr4::PAE);
        drop(vmcb);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPe));

        // LME & PG & PAE with CS.L and CS.D both set -> LongModeLAndD.
        let mut page = rung0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME);
        vmcb.set_cr0(cr0::PE | cr0::NE | cr0::PG);
        vmcb.set_cr4(cr4::PAE);
        let bad_cs = Seg {
            selector: 0x08,
            attr: SegAttr::from_fields(0xB, true, 0, true, false, true, true, false),
            limit: 0xFFFF_FFFF,
            base: 0,
        };
        vmcb.set_seg(save::CS, bad_cs);
        drop(vmcb);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LongModeLAndD));

        // ASID 0 -> AsidZero.
        let mut page = rung0_page();
        Vmcb::new(&mut page).set_asid(0);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::AsidZero));

        // VMRUN intercept clear -> VmrunNotIntercepted.
        let mut page = rung0_page();
        Vmcb::new(&mut page).set_intr1(intr1::VMMCALL);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::VmrunNotIntercepted));
    }

    /// The long-mode shape: rung 0's flat state with long mode switched
    /// on — guest paging up (`PG` + `PAE`), `LME` and `LMA` set, a 64-bit
    /// CS. This is the state the paged guest rung loads and what
    /// `LongModeLAndD` guards.
    fn long_mode_page() -> [u8; VMCB_SIZE] {
        let mut page = rung0_page();
        let mut vmcb = Vmcb::new(&mut page);
        vmcb.set_efer(msr::EFER_SVME | msr::EFER_LME | msr::EFER_LMA);
        vmcb.set_cr0(cr0::PE | cr0::NE | cr0::PG);
        vmcb.set_cr4(cr4::PAE);
        vmcb.set_seg(
            save::CS,
            Seg { selector: 0x08, attr: SegAttr::CODE64_DPL0, limit: 0xFFFF_FFFF, base: 0 },
        );
        page
    }

    /// L=1 with D=0 is the whole long-mode segment attribute: type `B`,
    /// S=1, P=1, raw word 0x29B (0xB + S + P + L).
    #[test]
    fn code64_attr_is_long_not_db() {
        let a = SegAttr::CODE64_DPL0;
        assert_eq!(a.ty(), 0xB);
        assert!(a.is_code_data());
        assert_eq!(a.dpl(), 0);
        assert!(a.is_present());
        assert!(a.is_long());
        assert!(!a.is_db());
        assert!(!a.is_4k_granular());
        assert_eq!(a.0, 0x29B);
    }

    #[test]
    fn long_mode_paged_guest_is_legal() {
        let mut page = long_mode_page();
        let vmcb = Vmcb::new(&mut page);
        assert_eq!(validate_guest_state(&vmcb), Ok(()));
    }

    /// The long-mode state minus one ingredient must fail the one check the
    /// missing ingredient belongs to — the exact failure a transposed bit
    /// in a VMCB builder would take.
    #[test]
    fn long_mode_guest_loses_legality_one_ingredient_at_a_time() {
        // No PAE -> LmePgWithoutPae.
        let mut page = long_mode_page();
        Vmcb::new(&mut page).set_cr4(0);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPae));

        // No PE -> LmePgWithoutPe.
        let mut page = long_mode_page();
        Vmcb::new(&mut page).set_cr0(cr0::NE | cr0::PG);
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LmePgWithoutPe));

        // CS.L and CS.D together -> LongModeLAndD.
        let mut page = long_mode_page();
        Vmcb::new(&mut page).set_seg(
            save::CS,
            Seg {
                selector: 0x08,
                attr: SegAttr::from_fields(0xB, true, 0, true, false, true, true, false),
                limit: 0xFFFF_FFFF,
                base: 0,
            },
        );
        assert_eq!(validate_guest_state(&Vmcb::new(&mut page)), Err(Illicit::LongModeLAndD));
    }

    #[test]
    fn npt_encodings_are_the_documented_subset() {
        assert_eq!(npt::pointer(0x1234_5000), 0x1234_5000 | 0b111);
        assert_eq!(npt::leaf_2m(0x200_0000), 0x200_0000 | npt::P | npt::RW | npt::US | npt::PS);
        assert_eq!(npt::leaf_2m(0x200_0000) & npt::PS != 0, true);
        assert_eq!(0x1234_5000 & npt::ADDR_MASK, 0x1234_5000);
    }

    #[test]
    fn illicit_names_are_distinct_serial_strings() {
        let all = [
            Illicit::SvmeClear,
            Illicit::Cr0CdWithoutNw,
            Illicit::Cr0HighBits,
            Illicit::Dr6HighBits,
            Illicit::Dr7HighBits,
            Illicit::LmaWithoutLme,
            Illicit::LmePgWithoutPae,
            Illicit::LmePgWithoutPe,
            Illicit::LongModeLAndD,
            Illicit::AsidZero,
            Illicit::VmrunNotIntercepted,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.name().is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a.name(), b.name());
            }
        }
    }

    /// §15.11 / Table 158: four 16-Kbit vectors, the three MSR ranges and
    /// the reserved fourth; two bits per MSR, lsb read, msb write.
    #[test]
    fn msrpm_offsets_match_table_158() {
        assert_eq!(msrpm::SIZE, 8192);
        assert_eq!(msrpm::VECTOR, 2048);
        // EFER (0xC000_0080) sits in the C000_0000h vector: byte
        // 0x800 + 0x80/4, bits (0x80 % 4)*2..+1.
        assert_eq!(msrpm::bits_of(0xC000_0080), Some((0x800 + 0x20, 0)));
        // First and last MSR of two areas — the byte/bit arithmetic.
        assert_eq!(msrpm::bits_of(0x0000_0000), Some((0, 0)));
        assert_eq!(msrpm::bits_of(0x0000_1FFF), Some((2047, 6)));
        assert_eq!(msrpm::bits_of(0xC001_0000), Some((2 * 2048, 0)));
        assert_eq!(msrpm::bits_of(0xC001_1FFF), Some((3 * 2048 - 1, 6)));
        // The reserved fourth vector (and every uncovered MSR) has no bits:
        // the auto-intercept, which `bits` reports as intercepted.
        assert_eq!(msrpm::bits_of(0x0000_2000), None);
        assert_eq!(msrpm::bits_of(0xC000_2000), None);
        assert_eq!(msrpm::bits_of(0xC001_2000), None);
        assert_eq!(msrpm::bits(&[0; msrpm::SIZE], 0xC000_2000), (true, true));
    }

    #[test]
    fn msrpm_intercept_bits_roundtrip() {
        let page = msrpm::with_access([0; msrpm::SIZE], msr::EFER, true, false);
        assert_eq!(msrpm::bits(&page, msr::EFER), (true, false));
        // The neighbouring MSR shares a byte — it must stay clear.
        assert_eq!(msrpm::bits(&page, 0xC000_0081), (false, false));
        let page = msrpm::with_access(page, 0xC000_0081, true, true);
        assert_eq!(msrpm::bits(&page, msr::EFER), (true, false));
        assert_eq!(msrpm::bits(&page, 0xC000_0081), (true, true));
        let page = msrpm::with_access(page, 0xC000_0081, false, false);
        assert_eq!(msrpm::bits(&page, 0xC000_0081), (false, false));
        assert_eq!(msrpm::bits(&page, msr::EFER), (true, false));
        // Arming a reserved-vector MSR is a no-op, not a panic.
        let _ = msrpm::with_access([0; msrpm::SIZE], 0xC000_4000, true, true);
    }

    /// §15.10.1: 12 contiguous KiB = 64K+3 bits, one bit per port,
    /// bit 0 = port 0, 1 = intercept.
    #[test]
    fn iopm_is_one_bit_per_port() {
        assert_eq!(iopm::SIZE, 12288);
        assert_eq!(iopm::PORTS, 65536);
        let page = iopm::with_port([0; iopm::SIZE], 0, true);
        assert!(iopm::intercepted(&page, 0));
        assert!(!iopm::intercepted(&page, 1));
        // Port 0x80 (POST): byte 16, bit 0.
        let page = iopm::with_port(page, 0x80, true);
        assert!(iopm::intercepted(&page, 0x80));
        assert_eq!(page[0x10], 0x01);
        // The last port: last byte, last bit.
        let page = iopm::with_port(page, u16::MAX, true);
        assert!(iopm::intercepted(&page, u16::MAX));
        assert_eq!(page[8191], 0x80);
        // Clearing one port of an all-intercept map clears only its bit.
        let page = iopm::with_port([0xFF; iopm::SIZE], 0x1234, false);
        assert!(!iopm::intercepted(&page, 0x1234));
        assert!(iopm::intercepted(&page, 0x1235));
    }
}
